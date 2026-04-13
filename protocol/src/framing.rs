use std::io;

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

use crate::Envelope;

#[derive(Debug)]
pub enum FrameError {
    Io(io::Error),
    Json(serde_json::Error),
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
