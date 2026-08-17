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
