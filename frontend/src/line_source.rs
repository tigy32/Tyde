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
}
