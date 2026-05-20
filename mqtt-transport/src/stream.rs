use std::io;
use std::pin::Pin;
use std::task::{Context, Poll, ready};

use futures_channel::mpsc::Sender;
use futures_util::Sink;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::mpsc::Receiver;

use crate::chunking::{InboundPlaintext, OutboundPlaintext, next_write_len};
use crate::error::MqttTransportError;

#[derive(Debug)]
pub(crate) enum InboundEvent {
    Data(Vec<u8>),
    Error(Box<MqttTransportError>),
    Eof,
}

pub struct EnvelopeStream {
    outbound_tx: Sender<Vec<u8>>,
    inbound_rx: Receiver<InboundEvent>,
    inbound: InboundPlaintext,
    outbound: OutboundPlaintext,
    pending_chunk: Option<Vec<u8>>,
    shutdown_started: bool,
}

impl EnvelopeStream {
    pub(crate) fn new(outbound_tx: Sender<Vec<u8>>, inbound_rx: Receiver<InboundEvent>) -> Self {
        Self {
            outbound_tx,
            inbound_rx,
            inbound: InboundPlaintext::new(),
            outbound: OutboundPlaintext::new(),
            pending_chunk: None,
            shutdown_started: false,
        }
    }

    fn poll_send_pending(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let Some(chunk) = self.pending_chunk.take() else {
            return Poll::Ready(Ok(()));
        };

        match Pin::new(&mut self.outbound_tx).poll_ready(cx) {
            Poll::Ready(Ok(())) => match Pin::new(&mut self.outbound_tx).start_send(chunk) {
                Ok(()) => Poll::Ready(Ok(())),
                Err(_) => Poll::Ready(Err(actor_closed_io_error())),
            },
            Poll::Ready(Err(_)) => Poll::Ready(Err(actor_closed_io_error())),
            Poll::Pending => {
                self.pending_chunk = Some(chunk);
                Poll::Pending
            }
        }
    }

    fn poll_send_chunk(&mut self, cx: &mut Context<'_>, chunk: Vec<u8>) -> Poll<io::Result<()>> {
        self.pending_chunk = Some(chunk);
        self.poll_send_pending(cx)
    }

    fn poll_send_full_chunks(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        ready!(self.poll_send_pending(cx))?;
        while let Some(chunk) = self.outbound.take_full_chunk() {
            ready!(self.poll_send_chunk(cx, chunk))?;
        }
        Poll::Ready(Ok(()))
    }
}

impl Unpin for EnvelopeStream {}

impl AsyncRead for EnvelopeStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }

        loop {
            if !self.inbound.is_empty() {
                let dest = buf.initialize_unfilled();
                let written = self.inbound.read_into(dest);
                buf.advance(written);
                return Poll::Ready(Ok(()));
            }

            match Pin::new(&mut self.inbound_rx).poll_recv(cx) {
                Poll::Ready(Some(InboundEvent::Data(chunk))) => {
                    if !chunk.is_empty() {
                        self.inbound.push_chunk(chunk);
                    }
                }
                Poll::Ready(Some(InboundEvent::Error(error))) => {
                    return Poll::Ready(Err(io::Error::other(*error)));
                }
                Poll::Ready(Some(InboundEvent::Eof)) | Poll::Ready(None) => {
                    return Poll::Ready(Ok(()));
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

impl AsyncWrite for EnvelopeStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if self.shutdown_started {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "EnvelopeStream has been shut down",
            )));
        }

        ready!(self.poll_send_full_chunks(cx))?;

        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }

        let write_len = next_write_len(self.outbound.buffered_len(), buf.len());
        self.outbound.append(&buf[..write_len]);
        Poll::Ready(Ok(write_len))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        ready!(self.poll_send_pending(cx))?;
        while let Some(chunk) = self.outbound.take_full_chunk() {
            ready!(self.poll_send_chunk(cx, chunk))?;
        }
        if let Some(chunk) = self.outbound.take_flush_chunk() {
            ready!(self.poll_send_chunk(cx, chunk))?;
        }
        match Pin::new(&mut self.outbound_tx).poll_flush(cx) {
            Poll::Ready(Ok(())) => Poll::Ready(Ok(())),
            Poll::Ready(Err(_)) => Poll::Ready(Err(actor_closed_io_error())),
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.shutdown_started = true;
        ready!(self.as_mut().poll_flush(cx))?;
        match Pin::new(&mut self.outbound_tx).poll_close(cx) {
            Poll::Ready(Ok(())) => Poll::Ready(Ok(())),
            Poll::Ready(Err(_)) => Poll::Ready(Err(actor_closed_io_error())),
            Poll::Pending => Poll::Pending,
        }
    }
}

fn actor_closed_io_error() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, MqttTransportError::ActorClosed)
}
