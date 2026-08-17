//! `Content-Length` framing for LSP / JSON-RPC over stdio.
//!
//! This is **not** the newline-delimited NDJSON framing the tycode subprocess
//! uses (`server/src/backend/subprocess.rs`, `protocol/src/framing.rs`). LSP
//! frames each message with a header block terminated by a blank line, e.g.
//!
//! ```text
//! Content-Length: 42\r\n
//! \r\n
//! {"jsonrpc":"2.0", ... }
//! ```
//!
//! so the subprocess line reader is not reusable — hence this dedicated codec.
//!
//! [`encode`] writes one message; [`LspDecoder`] is a stateful, push-based
//! decoder that tolerates partial reads (a header or body split across multiple
//! `read()` calls) and multiple whole messages buffered in a single read.

use serde_json::Value;

/// A framing/parse failure in the provider's LSP traffic. Surfaced, never
/// swallowed — malformed traffic maps to `CodeIntelErrorCode::ProtocolError`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LspCodecError {
    /// The header block was present but had no valid `Content-Length`.
    MissingContentLength,
    /// `Content-Length` was present but not a base-10 integer.
    InvalidContentLength(String),
    /// The message body was not valid JSON.
    InvalidJson(String),
    /// The header block grew past [`MAX_HEADER_SIZE`] with no terminator — a
    /// malformed or runaway stream, not a partial read.
    HeaderTooLarge,
    /// `Content-Length` (or `body_start + length`) exceeds [`MAX_MESSAGE_SIZE`].
    MessageTooLarge(usize),
}

impl std::fmt::Display for LspCodecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LspCodecError::MissingContentLength => {
                f.write_str("LSP header block missing Content-Length")
            }
            LspCodecError::InvalidContentLength(raw) => {
                write!(f, "invalid LSP Content-Length: {raw:?}")
            }
            LspCodecError::InvalidJson(err) => write!(f, "invalid LSP message body: {err}"),
            LspCodecError::HeaderTooLarge => write!(
                f,
                "LSP header block exceeded {MAX_HEADER_SIZE} bytes with no terminator"
            ),
            LspCodecError::MessageTooLarge(len) => {
                write!(
                    f,
                    "LSP message size {len} exceeds the {MAX_MESSAGE_SIZE} byte cap"
                )
            }
        }
    }
}

impl std::error::Error for LspCodecError {}

/// Upper bound on the header block (everything before the blank line). LSP
/// headers are tiny (`Content-Length` + maybe `Content-Type`); anything past
/// this is a malformed/runaway stream, so we reject rather than buffer forever.
const MAX_HEADER_SIZE: usize = 16 * 1024;

/// Upper bound on a single framed message (`body_start + Content-Length`).
/// rust-analyzer messages are large (full semantic models) but bounded; 64 MiB
/// is comfortably above any real message and caps a hostile/garbage length.
const MAX_MESSAGE_SIZE: usize = 64 * 1024 * 1024;

/// Serialize a JSON-RPC message into a `Content-Length`-framed byte vector,
/// ready to write to the language server's stdin.
pub(crate) fn encode(message: &Value) -> Vec<u8> {
    let body = serde_json::to_vec(message).expect("serde_json::Value always serializes");
    let mut framed = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    framed.extend_from_slice(&body);
    framed
}

/// The four-byte header/body separator.
const HEADER_TERMINATOR: &[u8] = b"\r\n\r\n";

/// Stateful incremental decoder. Feed it bytes via [`extend`](Self::extend) as
/// they arrive on stdout, then drain whole messages via [`next`](Self::next).
#[derive(Default)]
pub(crate) struct LspDecoder {
    buf: Vec<u8>,
}

impl LspDecoder {
    pub(crate) fn new() -> Self {
        Self { buf: Vec::new() }
    }

    /// Append freshly-read bytes to the internal buffer.
    pub(crate) fn extend(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Try to decode one complete message from the buffered bytes.
    ///
    /// - `Ok(Some(value))` — a whole message was framed and removed from the
    ///   buffer. Call again to drain any further buffered messages.
    /// - `Ok(None)` — not enough bytes yet (partial header or partial body);
    ///   the buffer is left intact for the next `extend`.
    /// - `Err(_)` — the buffered bytes are malformed (the affected message is
    ///   consumed so the caller can surface the error and continue or abort).
    pub(crate) fn next(&mut self) -> Result<Option<Value>, LspCodecError> {
        // Find the end of the header block. Until it arrives we can't know the
        // body length, so hold the bytes — but cap how much we'll buffer
        // looking for the terminator so a stream that never sends one (or a
        // flood of header bytes) can't grow the buffer without bound.
        let Some(header_end) = find_subslice(&self.buf, HEADER_TERMINATOR) else {
            if self.buf.len() > MAX_HEADER_SIZE {
                self.buf.clear();
                return Err(LspCodecError::HeaderTooLarge);
            }
            return Ok(None);
        };
        let body_start = header_end + HEADER_TERMINATOR.len();

        let content_length = match parse_content_length(&self.buf[..header_end]) {
            Ok(len) => len,
            Err(error) => {
                // Consume the bad header so a retry doesn't re-hit it forever.
                self.buf.drain(..body_start);
                return Err(error);
            }
        };

        // Guard the total framed size: reject before allocating/buffering, and
        // use checked arithmetic so a hostile Content-Length can't overflow the
        // `body_start + content_length` index.
        let total = match body_start.checked_add(content_length) {
            Some(total) if total <= MAX_MESSAGE_SIZE => total,
            _ => {
                self.buf.drain(..body_start);
                return Err(LspCodecError::MessageTooLarge(content_length));
            }
        };

        if self.buf.len() < total {
            // Body not fully arrived yet.
            return Ok(None);
        }

        let body: Vec<u8> = self.buf.drain(..total).skip(body_start).collect();

        match serde_json::from_slice::<Value>(&body) {
            Ok(value) => Ok(Some(value)),
            Err(error) => Err(LspCodecError::InvalidJson(error.to_string())),
        }
    }
}

/// Parse the `Content-Length` value out of a header block (everything before
/// the terminating blank line). Headers are ASCII, case-insensitive field
/// names per the LSP base protocol; other headers (e.g. `Content-Type`) are
/// ignored.
fn parse_content_length(header_block: &[u8]) -> Result<usize, LspCodecError> {
    let headers = String::from_utf8_lossy(header_block);
    for line in headers.split("\r\n") {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case("content-length") {
            let trimmed = value.trim();
            return trimmed
                .parse::<usize>()
                .map_err(|_| LspCodecError::InvalidContentLength(trimmed.to_owned()));
        }
    }
    Err(LspCodecError::MissingContentLength)
}

/// First index of `needle` within `haystack`, or `None`.
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}
