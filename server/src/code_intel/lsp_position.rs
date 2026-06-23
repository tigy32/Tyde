//! UTF-16 ↔ UTF-8-byte position conversion, confined to the provider.
//!
//! LSP `Position` is `{ line, character }` where `line` is a 0-based line index
//! and `character` is a 0-based offset in **UTF-16 code units** within that line
//! (the default `PositionEncodingKind`). Tyde's wire protocol speaks **absolute
//! file byte offsets** (`ByteRange`, half-open `[start, end)`), matching
//! `ProjectSearchMatch.ranges` and the `FileLines` byte model. This module is
//! the *only* place that conversion happens — the frontend never sees UTF-16.
//!
//! This is the #1 silent-correctness hazard in the whole feature, so it is
//! property-tested against adversarial inputs: multibyte UTF-8 (emoji, CJK),
//! combining marks, astral-plane characters that occupy two UTF-16 code units,
//! and CRLF line endings.

use protocol::ByteRange;

/// Precomputed line-start byte offsets for one file's text, so repeated
/// position lookups (every diagnostic range start + end) are cheap.
pub(crate) struct LineIndex<'a> {
    text: &'a str,
    /// Byte offset where each line starts. Lines are split on `\n`; a trailing
    /// `\r` stays attached to the preceding line (LSP counts characters up to,
    /// but not including, the line terminator, and `\r` is one UTF-16 unit and
    /// one byte, so it converts transparently). Length is `num_lines`.
    line_starts: Vec<u32>,
}

impl<'a> LineIndex<'a> {
    pub(crate) fn new(text: &'a str) -> Self {
        let mut line_starts = vec![0u32];
        for (i, byte) in text.bytes().enumerate() {
            if byte == b'\n' {
                line_starts.push((i + 1) as u32);
            }
        }
        Self { text, line_starts }
    }

    /// Slice of bytes belonging to line `line` (0-based), excluding the
    /// terminating `\n` but **including** any `\r` before it.
    fn line_str(&self, line: u32) -> &'a str {
        let line = line as usize;
        let start = self.line_starts[line] as usize;
        let end = self
            .line_starts
            .get(line + 1)
            .map(|next| {
                // Exclude the trailing '\n' of this line.
                let next = *next as usize;
                if next > start && self.text.as_bytes()[next - 1] == b'\n' {
                    next - 1
                } else {
                    next
                }
            })
            .unwrap_or(self.text.len());
        &self.text[start..end]
    }

    /// Convert an LSP `(line, character_utf16)` position to an absolute file
    /// byte offset.
    ///
    /// Out-of-range inputs are clamped to the nearest valid boundary (a line
    /// past EOF clamps to the file length; a character past the end of its line
    /// clamps to the line end) rather than panicking — a language server should
    /// never send those, but a malformed position must not crash the provider.
    pub(crate) fn position_to_byte(&self, line: u32, character_utf16: u32) -> u32 {
        if (line as usize) >= self.line_starts.len() {
            return self.text.len() as u32;
        }
        let line_start = self.line_starts[line as usize];
        let line_text = self.line_str(line);

        let mut utf16_seen = 0u32;
        for (byte_offset, ch) in line_text.char_indices() {
            if utf16_seen >= character_utf16 {
                return line_start + byte_offset as u32;
            }
            utf16_seen += ch.len_utf16() as u32;
        }
        // `character` is at or past the end of the line's content.
        line_start + line_text.len() as u32
    }

    /// Convert an absolute file byte offset to an LSP `(line, character_utf16)`
    /// position — the inverse of [`position_to_byte`](Self::position_to_byte).
    /// This is the request side: a Tyde byte offset (from a click / caret) is
    /// turned into the UTF-16 position rust-analyzer expects for
    /// `textDocument/definition` / `textDocument/hover`.
    ///
    /// An offset past EOF clamps to the file length; an offset landing inside a
    /// multibyte char clamps down to that char's start (its preceding boundary),
    /// so a malformed offset never panics or slices mid-char.
    pub(crate) fn byte_to_position(&self, byte: u32) -> (u32, u32) {
        let byte = byte.min(self.text.len() as u32);
        // The line is the last line whose start is <= byte. `line_starts` is
        // sorted ascending, so binary-search and step back on an inexact hit.
        let line = match self.line_starts.binary_search(&byte) {
            Ok(exact) => exact,
            Err(insert) => insert.saturating_sub(1),
        };
        let line_start = self.line_starts[line];
        let line_text = self.line_str(line as u32);
        let target_in_line = (byte - line_start) as usize;

        let mut utf16 = 0u32;
        for (offset, ch) in line_text.char_indices() {
            // Count only chars that lie fully before the target byte. If the
            // target falls inside a multibyte char, clamp to that char's start
            // (don't count it) rather than slicing it.
            if offset + ch.len_utf8() > target_in_line {
                break;
            }
            utf16 += ch.len_utf16() as u32;
        }
        (line as u32, utf16)
    }

    /// The 0-based `line`'s start byte offset and its text (excluding the
    /// terminating `\n`, but including any trailing `\r`). `None` when `line` is
    /// past the end of the file. Used by find-references to slice a per-line
    /// preview and convert absolute byte ranges into line-relative ones.
    pub(crate) fn line_span(&self, line: u32) -> Option<(u32, &'a str)> {
        if (line as usize) >= self.line_starts.len() {
            return None;
        }
        Some((self.line_starts[line as usize], self.line_str(line)))
    }

    /// Convert an LSP range to a Tyde half-open [`ByteRange`].
    pub(crate) fn range_to_byte_range(
        &self,
        start_line: u32,
        start_char: u32,
        end_line: u32,
        end_char: u32,
    ) -> ByteRange {
        ByteRange {
            start: self.position_to_byte(start_line, start_char),
            end: self.position_to_byte(end_line, end_char),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Independent reference implementation: walk the whole string tracking
    /// (line, utf16-col, byte) so a property test can cross-check
    /// `position_to_byte` against a from-scratch computation.
    fn reference_positions(text: &str) -> Vec<(u32, u32, u32)> {
        // (line, utf16_character, byte_offset) for every char boundary,
        // including the position just before each `\n` and at line starts.
        let mut out = Vec::new();
        let mut line = 0u32;
        let mut col_utf16 = 0u32;
        let mut byte = 0u32;
        out.push((line, col_utf16, byte));
        for ch in text.chars() {
            if ch == '\n' {
                line += 1;
                col_utf16 = 0;
                byte += ch.len_utf8() as u32;
                out.push((line, col_utf16, byte));
            } else {
                col_utf16 += ch.len_utf16() as u32;
                byte += ch.len_utf8() as u32;
                out.push((line, col_utf16, byte));
            }
        }
        out
    }

    fn check_all_positions(text: &str) {
        let index = LineIndex::new(text);
        for (line, col_utf16, expected_byte) in reference_positions(text) {
            // A position at the very end of a line that is followed by '\n'
            // maps to the byte just before that '\n' (line content excludes the
            // terminator). The reference's post-'\n' entry is the *next* line's
            // start, which `position_to_byte` reproduces from (line, 0).
            let got = index.position_to_byte(line, col_utf16);
            assert_eq!(
                got, expected_byte,
                "text={text:?} line={line} col_utf16={col_utf16}: got {got}, want {expected_byte}"
            );
        }
    }

    #[test]
    fn ascii_single_line() {
        check_all_positions("fn main() {}");
    }

    #[test]
    fn ascii_multi_line() {
        check_all_positions("fn main() {\n    let x = 1;\n}\n");
    }

    #[test]
    fn emoji_astral_plane_two_utf16_units() {
        // "😀" is U+1F600: 4 UTF-8 bytes, 2 UTF-16 code units (surrogate pair).
        let text = "let s = \"😀x\";";
        check_all_positions(text);
        let index = LineIndex::new(text);
        // Byte offset of the char *after* the emoji: prefix `let s = "` is 9
        // bytes, emoji is 4 bytes → 'x' starts at byte 13, at UTF-16 col 9+2=11.
        assert_eq!(index.position_to_byte(0, 11), 13);
    }

    #[test]
    fn cjk_three_byte_one_utf16_unit() {
        // CJK ideographs are 3 UTF-8 bytes and 1 UTF-16 code unit each.
        check_all_positions("let 名前 = 値;");
    }

    #[test]
    fn combining_marks() {
        // 'e' + U+0301 (combining acute). Two scalar values, each 1 UTF-16 unit;
        // the combining mark is 2 UTF-8 bytes.
        check_all_positions("café\ncombining: e\u{0301}!");
    }

    #[test]
    fn crlf_line_endings() {
        // '\r' stays attached to the line; positions before it still convert.
        let text = "line one\r\nsecond 😀 line\r\n";
        check_all_positions(text);
        let index = LineIndex::new(text);
        // Line 1, col 0 is the byte right after the first "\r\n" (10 bytes).
        assert_eq!(index.position_to_byte(1, 0), 10);
    }

    #[test]
    fn character_past_line_end_clamps_to_line_end() {
        let text = "ab\ncd";
        let index = LineIndex::new(text);
        // Line 0 has 2 chars; asking for col 99 clamps to the '\n' position.
        assert_eq!(index.position_to_byte(0, 99), 2);
    }

    #[test]
    fn line_past_eof_clamps_to_file_len() {
        let text = "abc";
        let index = LineIndex::new(text);
        assert_eq!(index.position_to_byte(50, 0), 3);
    }

    /// `byte_to_position` must invert `position_to_byte` at every char
    /// boundary: feeding each reference byte offset back yields the same
    /// `(line, utf16)` the reference computed. Covers the request side
    /// (Tyde byte offset → LSP position) on adversarial multibyte input.
    fn check_byte_to_position(text: &str) {
        let index = LineIndex::new(text);
        for (line, col_utf16, byte) in reference_positions(text) {
            let got = index.byte_to_position(byte);
            assert_eq!(
                got,
                (line, col_utf16),
                "text={text:?} byte={byte}: got {got:?}, want ({line}, {col_utf16})"
            );
        }
    }

    #[test]
    fn byte_to_position_inverts_position_to_byte() {
        check_byte_to_position("fn main() {}");
        check_byte_to_position("fn main() {\n    let x = 1;\n}\n");
        check_byte_to_position("let s = \"😀x\";");
        check_byte_to_position("let 名前 = 値;");
        check_byte_to_position("café\ncombining: e\u{0301}!");
        check_byte_to_position("line one\r\nsecond 😀 line\r\n");
    }

    #[test]
    fn byte_to_position_clamps_offset_inside_multibyte_char() {
        // "名" is 3 UTF-8 bytes at byte 0. An offset of 1 or 2 lands *inside* the
        // char; it must clamp down to the char start (utf16 col 0), never panic.
        let index = LineIndex::new("名b");
        assert_eq!(index.byte_to_position(0), (0, 0));
        assert_eq!(index.byte_to_position(1), (0, 0));
        assert_eq!(index.byte_to_position(2), (0, 0));
        assert_eq!(index.byte_to_position(3), (0, 1)); // 'b'
    }

    #[test]
    fn byte_to_position_past_eof_clamps() {
        let index = LineIndex::new("abc\nde");
        assert_eq!(index.byte_to_position(999), (1, 2));
    }

    #[test]
    fn range_round_trips() {
        let text = "let 名前 = \"😀\";\nok";
        let index = LineIndex::new(text);
        let range = index.range_to_byte_range(0, 4, 0, 6);
        // cols 4..6 (UTF-16) cover the two CJK chars "名前": byte 4..10.
        assert_eq!(range, ByteRange { start: 4, end: 10 });
    }

    /// Deterministic pseudo-random property test (no `rand`/`proptest`
    /// dependency available). A small xorshift PRNG builds adversarial strings
    /// out of a mixed alphabet and the cross-check above validates every char
    /// boundary in each one.
    #[test]
    fn property_random_mixed_strings() {
        let alphabet = [
            "a",
            "Z",
            " ",
            "\n",
            "\r\n",
            "é",
            "名",
            "😀",
            "👨‍👩‍👧",
            "e\u{0301}",
            "\t",
            "{",
            "}",
        ];
        let mut state: u64 = 0x9E3779B97F4A7C15;
        let mut next = || {
            // xorshift64
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..400 {
            let len = (next() % 40) as usize;
            let mut s = String::new();
            for _ in 0..len {
                let pick = (next() as usize) % alphabet.len();
                s.push_str(alphabet[pick]);
            }
            check_all_positions(&s);
        }
    }
}
