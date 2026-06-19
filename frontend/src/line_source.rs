//! Line-source abstraction shared between `file_view`, `diff_view`, and
//! `FindState`. Two backings:
//!
//! - `LineSource::File` wraps `FileLines` — an `Arc<str>` of the whole file
//!   plus a `Vec<u32>` of byte offsets. No per-line `String` allocation;
//!   slices are produced on demand. Critical for huge files: opening a
//!   50 000-line file used to allocate 50 000 separate `String`s, which
//!   takes seconds in debug-build wasm.
//! - `LineSource::Owned` wraps `Arc<Vec<String>>` — used by the diff
//!   viewer, which builds its searchable line list by collecting per-hunk
//!   line texts from the protocol payload.
//!
//! Both variants are cheap to clone (`Arc`-bumped) and `Send + Sync` so
//! they can live inside Leptos signals / memo closures.

use std::sync::Arc;

/// Lazy view over an entire file's text plus line byte offsets.
#[derive(Clone)]
pub struct FileLines {
    text: Arc<str>,
    /// Byte offsets where each line *starts*. Length is `num_lines + 1`;
    /// the last entry is `text.len()` so `line(i)` can compute the end of
    /// the last line without bounds-checking a separate length.
    starts: Arc<Vec<u32>>,
}

impl FileLines {
    /// Build from a borrowed file content. Single allocation for the
    /// `Arc<str>`, single allocation for the `Vec<u32>`.
    pub fn new(content: &str) -> Self {
        // One pass to find newline positions; one allocation for the
        // offset vec sized to fit.
        let nl_count = content.bytes().filter(|&b| b == b'\n').count();
        let mut starts: Vec<u32> = Vec::with_capacity(nl_count + 2);
        starts.push(0);
        for (i, b) in content.bytes().enumerate() {
            if b == b'\n' {
                starts.push((i + 1) as u32);
            }
        }
        // Sentinel: end of last line. Avoids special-casing `line(last)`.
        if starts.last().copied().unwrap_or(0) as usize != content.len() {
            starts.push(content.len() as u32);
        }
        Self {
            text: Arc::from(content),
            starts: Arc::new(starts),
        }
    }

    pub fn len(&self) -> usize {
        // starts has num_lines + 1 entries (the trailing sentinel).
        self.starts.len().saturating_sub(1)
    }

    /// Absolute byte offset where line `i` starts. Panics if `i >= self.len()`.
    /// Used to map an absolute file byte range (e.g. a code-intel diagnostic)
    /// into per-line offsets for decoration overlays.
    pub fn line_start(&self, i: usize) -> u32 {
        self.starts[i]
    }

    /// Absolute byte offset of the end of line `i`'s *content* — the trailing
    /// `\n` (if any) is excluded, matching [`line`](Self::line). Panics if
    /// `i >= self.len()`.
    pub fn line_content_end(&self, i: usize) -> u32 {
        let start = self.starts[i] as usize;
        let raw_end = self.starts[i + 1] as usize;
        let end = if raw_end > start && self.text.as_bytes()[raw_end - 1] == b'\n' {
            raw_end - 1
        } else {
            raw_end
        };
        end as u32
    }

    /// The 0-based index of the line containing absolute byte offset `byte`.
    /// A `byte` at or past EOF clamps to the last line; an empty file yields 0.
    /// `starts` is sorted, so this is a binary search.
    pub fn line_for_byte(&self, byte: u32) -> usize {
        let line_count = self.len();
        if line_count == 0 {
            return 0;
        }
        // Find the last line whose start is <= byte.
        match self.starts[..line_count].binary_search(&byte) {
            Ok(exact) => exact,
            Err(insert) => insert.saturating_sub(1),
        }
    }

    /// Slice the file bytes for line `i`. Trailing `\n` is excluded so the
    /// returned slice contains just the line's text. Panics if `i >=
    /// self.len()` — callers iterate bounded by `len()`.
    pub fn line(&self, i: usize) -> &str {
        let start = self.starts[i] as usize;
        let raw_end = self.starts[i + 1] as usize;
        // The slice from start..raw_end includes the trailing newline (if
        // any). Trim it so callers see the visible line text only.
        let end = if raw_end > start && self.text.as_bytes()[raw_end - 1] == b'\n' {
            raw_end - 1
        } else {
            raw_end
        };
        &self.text[start..end]
    }
}

/// Line-source abstraction. Cheap to clone; consume via `len()` + `line(i)`.
#[derive(Clone)]
pub enum LineSource {
    File(FileLines),
    Owned(Arc<Vec<String>>),
}

impl LineSource {
    pub fn len(&self) -> usize {
        match self {
            Self::File(f) => f.len(),
            Self::Owned(v) => v.len(),
        }
    }

    pub fn line(&self, i: usize) -> &str {
        match self {
            Self::File(f) => f.line(i),
            Self::Owned(v) => v[i].as_str(),
        }
    }
}

impl From<FileLines> for LineSource {
    fn from(f: FileLines) -> Self {
        Self::File(f)
    }
}

impl From<Arc<Vec<String>>> for LineSource {
    fn from(v: Arc<Vec<String>>) -> Self {
        Self::Owned(v)
    }
}

impl From<Vec<String>> for LineSource {
    fn from(v: Vec<String>) -> Self {
        Self::Owned(Arc::new(v))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_lines_basic() {
        let f = FileLines::new("hello\nworld\nfoo");
        assert_eq!(f.len(), 3);
        assert_eq!(f.line(0), "hello");
        assert_eq!(f.line(1), "world");
        assert_eq!(f.line(2), "foo");
    }

    #[test]
    fn file_lines_trailing_newline() {
        let f = FileLines::new("a\nb\n");
        assert_eq!(f.len(), 2);
        assert_eq!(f.line(0), "a");
        assert_eq!(f.line(1), "b");
    }

    #[test]
    fn file_lines_empty() {
        let f = FileLines::new("");
        assert_eq!(f.len(), 0);
    }

    #[test]
    fn file_lines_single_line() {
        let f = FileLines::new("just one");
        assert_eq!(f.len(), 1);
        assert_eq!(f.line(0), "just one");
    }

    #[test]
    fn line_source_owned_dispatch() {
        let src: LineSource = vec!["a".to_owned(), "b".to_owned()].into();
        assert_eq!(src.len(), 2);
        assert_eq!(src.line(0), "a");
        assert_eq!(src.line(1), "b");
    }

    #[test]
    fn line_source_file_dispatch() {
        let f = FileLines::new("x\ny\nz");
        let src: LineSource = f.into();
        assert_eq!(src.len(), 3);
        assert_eq!(src.line(2), "z");
    }

    #[test]
    fn line_start_and_content_end() {
        let f = FileLines::new("ab\ncde\nf");
        assert_eq!(f.line_start(0), 0);
        assert_eq!(f.line_content_end(0), 2); // "ab", excludes '\n' at byte 2
        assert_eq!(f.line_start(1), 3);
        assert_eq!(f.line_content_end(1), 6); // "cde"
        assert_eq!(f.line_start(2), 7);
        assert_eq!(f.line_content_end(2), 8); // "f", no trailing newline
    }

    #[test]
    fn line_for_byte_maps_offsets_to_lines() {
        let f = FileLines::new("ab\ncde\nf");
        // line 0 spans bytes 0..3 (incl '\n'), line 1 bytes 3..7, line 2 7..8.
        assert_eq!(f.line_for_byte(0), 0);
        assert_eq!(f.line_for_byte(2), 0); // the '\n'
        assert_eq!(f.line_for_byte(3), 1); // 'c'
        assert_eq!(f.line_for_byte(6), 1); // the second '\n'
        assert_eq!(f.line_for_byte(7), 2); // 'f'
        assert_eq!(f.line_for_byte(999), 2); // past EOF clamps to last line
    }

    #[test]
    fn line_for_byte_empty_file() {
        let f = FileLines::new("");
        assert_eq!(f.line_for_byte(0), 0);
    }
}
