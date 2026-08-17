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
