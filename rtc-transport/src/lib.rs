use std::collections::VecDeque;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use futures_util::Sink;
use hmac::{Hmac, Mac};
use protocol::types::{MobileRtcDescription, MobileRtcSessionId, MobileSdpKind};
use sha2::Sha256;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::{mpsc, oneshot};

#[cfg(not(target_arch = "wasm32"))]
mod native;
#[cfg(not(target_arch = "wasm32"))]
pub use native::Peer;
#[cfg(target_arch = "wasm32")]
mod browser;
#[cfg(target_arch = "wasm32")]
pub use browser::Peer;

mod signaling;
pub use signaling::connect;

const WINDOW_CHUNKS: usize = 4;
const CHUNK_BYTES: usize = 16 * 1024;
const BUFFER_BYTES: usize = 256 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("WebRTC {stage}: {message}")]
    Connection {
        stage: &'static str,
        message: String,
    },
    #[error("WebRTC peer authentication failed")]
    Authentication,
    #[error("WebRTC expected {expected:?}, received {actual:?}")]
    DescriptionKind {
        expected: MobileSdpKind,
        actual: MobileSdpKind,
    },
    #[error("WebRTC signaling session does not match this connection")]
    SessionMismatch,
}

fn failure(stage: &'static str, error: impl std::fmt::Display) -> Error {
    Error::Connection {
        stage,
        message: error.to_string(),
    }
}

pub fn authenticate_description(
    session_id: MobileRtcSessionId,
    kind: MobileSdpKind,
    sdp: String,
    pairing_key: &[u8; 32],
) -> Result<MobileRtcDescription, Error> {
    let mut description = MobileRtcDescription {
        session_id,
        kind,
        sdp,
        authentication: String::new(),
    };
    description.authentication = URL_SAFE_NO_PAD.encode(
        description_mac(&description, pairing_key)?
            .finalize()
            .into_bytes(),
    );
    Ok(description)
}

pub fn verify_description(
    description: &MobileRtcDescription,
    session_id: &MobileRtcSessionId,
    expected: MobileSdpKind,
    pairing_key: &[u8; 32],
) -> Result<(), Error> {
    if &description.session_id != session_id {
        return Err(Error::SessionMismatch);
    }
    if description.kind != expected {
        return Err(Error::DescriptionKind {
            expected,
            actual: description.kind,
        });
    }
    let authentication = URL_SAFE_NO_PAD
        .decode(&description.authentication)
        .map_err(|_| Error::Authentication)?;
    description_mac(description, pairing_key)?
        .verify_slice(&authentication)
        .map_err(|_| Error::Authentication)
}

fn description_mac(
    description: &MobileRtcDescription,
    pairing_key: &[u8; 32],
) -> Result<Hmac<Sha256>, Error> {
    let bytes = serde_json::to_vec(&(
        "tyde-rtc-description-v1",
        &description.session_id,
        description.kind,
        &description.sdp,
    ))
    .map_err(|error| failure("encode peer authentication", error))?;
    let mut mac = Hmac::<Sha256>::new_from_slice(pairing_key).map_err(|_| Error::Authentication)?;
    mac.update(&bytes);
    Ok(mac)
}

pub struct RtcStream {
    outbound: futures_channel::mpsc::Sender<Outbound>,
    inbound: mpsc::Receiver<Result<Vec<u8>, String>>,
    partial: Vec<u8>,
    offset: usize,
    acknowledgements: VecDeque<oneshot::Receiver<Result<(), String>>>,
    abort: futures_util::future::AbortHandle,
}

struct Outbound {
    bytes: Vec<u8>,
    accepted: oneshot::Sender<Result<(), String>>,
}

struct StreamEndpoint {
    outbound: futures_channel::mpsc::Receiver<Outbound>,
    inbound: mpsc::Sender<Result<Vec<u8>, String>>,
}

fn stream_pair() -> (
    RtcStream,
    StreamEndpoint,
    futures_util::future::AbortRegistration,
) {
    let (outbound, output) = futures_channel::mpsc::channel(WINDOW_CHUNKS);
    let (input, inbound) = mpsc::channel(BUFFER_BYTES / CHUNK_BYTES);
    let (abort, registration) = futures_util::future::AbortHandle::new_pair();
    (
        RtcStream {
            outbound,
            inbound,
            partial: Vec::new(),
            offset: 0,
            acknowledgements: VecDeque::new(),
            abort,
        },
        StreamEndpoint {
            outbound: output,
            inbound: input,
        },
        registration,
    )
}

fn io_failure(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::ConnectionAborted, message.into())
}

impl AsyncRead for RtcStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        loop {
            if self.offset < self.partial.len() {
                let count = buf.remaining().min(self.partial.len() - self.offset);
                buf.put_slice(&self.partial[self.offset..self.offset + count]);
                self.offset += count;
                return Poll::Ready(Ok(()));
            }
            match std::task::ready!(self.inbound.poll_recv(cx)) {
                Some(Ok(bytes)) => {
                    self.partial = bytes;
                    self.offset = 0;
                }
                Some(Err(message)) => return Poll::Ready(Err(io_failure(message))),
                None => return Poll::Ready(Ok(())),
            }
        }
    }
}

impl AsyncWrite for RtcStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        if self.acknowledgements.len() >= WINDOW_CHUNKS {
            std::task::ready!(self.as_mut().poll_flush(cx))?;
        }
        std::task::ready!(Pin::new(&mut self.outbound).poll_ready(cx))
            .map_err(|error| io_failure(error.to_string()))?;
        let count = buf.len().min(CHUNK_BYTES);
        let (accepted, acknowledgement) = oneshot::channel();
        Pin::new(&mut self.outbound)
            .start_send(Outbound {
                bytes: buf[..count].to_vec(),
                accepted,
            })
            .map_err(|error| io_failure(error.to_string()))?;
        self.acknowledgements.push_back(acknowledgement);
        Poll::Ready(Ok(count))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        while let Some(acknowledgement) = self.acknowledgements.front_mut() {
            std::task::ready!(Pin::new(acknowledgement).poll(cx))
                .map_err(|error| io_failure(error.to_string()))?
                .map_err(io_failure)?;
            self.acknowledgements.pop_front();
        }
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        std::task::ready!(self.as_mut().poll_flush(cx))?;
        Pin::new(&mut self.outbound)
            .poll_close(cx)
            .map_err(|error| io_failure(error.to_string()))
    }
}

impl Drop for RtcStream {
    fn drop(&mut self) {
        self.abort.abort();
    }
}
