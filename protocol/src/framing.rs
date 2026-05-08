use std::io;

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

use crate::Envelope;
use crate::types::SeqMismatch;

#[derive(Debug)]
pub enum FrameError {
    Io(io::Error),
    Json(serde_json::Error),
    /// Peer sent a frame that violates the wire protocol (e.g. per-stream
    /// sequence numbers out of order). Callers should close the connection.
    Protocol(String),
}

impl From<io::Error> for FrameError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<serde_json::Error> for FrameError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

impl From<SeqMismatch> for FrameError {
    fn from(value: SeqMismatch) -> Self {
        Self::Protocol(value.to_string())
    }
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(err) => write!(f, "io error: {err}"),
            Self::Json(err) => write!(f, "json error: {err}"),
            Self::Protocol(msg) => write!(f, "protocol violation: {msg}"),
        }
    }
}

impl std::error::Error for FrameError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            Self::Json(err) => Some(err),
            Self::Protocol(_) => None,
        }
    }
}

pub async fn write_envelope<W: AsyncWrite + Unpin>(
    w: &mut W,
    envelope: &Envelope,
) -> Result<(), FrameError> {
    let mut bytes = serde_json::to_vec(envelope)?;
    bytes.push(b'\n');
    w.write_all(&bytes).await?;
    w.flush().await?;
    Ok(())
}

pub async fn read_envelope<R: AsyncBufRead + Unpin>(
    r: &mut R,
) -> Result<Option<Envelope>, FrameError> {
    let mut line = String::new();
    let read = r.read_line(&mut line).await?;
    if read == 0 {
        return Ok(None);
    }

    if line.ends_with('\n') {
        line.pop();
        if line.ends_with('\r') {
            line.pop();
        }
    }

    Ok(Some(serde_json::from_str(&line)?))
}
