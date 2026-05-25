use std::collections::VecDeque;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll, ready};

use futures_channel::mpsc::Sender;
use futures_util::Sink;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::{mpsc::Receiver, oneshot};

use crate::chunking::{InboundPlaintext, OutboundPlaintext, next_write_len};
use crate::error::MqttTransportError;

#[derive(Debug)]
pub(crate) enum InboundEvent {
    Data(Vec<u8>),
    Error(Box<MqttTransportError>),
    Eof,
}

pub(crate) struct OutboundChunk {
    pub bytes: Vec<u8>,
    pub ack: oneshot::Sender<Result<(), String>>,
}

struct PendingChunk {
    bytes: Vec<u8>,
    ack_tx: Option<oneshot::Sender<Result<(), String>>>,
    ack_rx: Option<oneshot::Receiver<Result<(), String>>>,
}

impl PendingChunk {
    fn new(bytes: Vec<u8>) -> Self {
        let (ack_tx, ack_rx) = oneshot::channel();
        Self {
            bytes,
            ack_tx: Some(ack_tx),
            ack_rx: Some(ack_rx),
        }
    }

    fn take_outbound(&mut self) -> io::Result<OutboundChunk> {
        let Some(ack) = self.ack_tx.take() else {
            return Err(io::Error::other(
                "pending outbound chunk missing ack sender",
            ));
        };
        Ok(OutboundChunk {
            bytes: std::mem::take(&mut self.bytes),
            ack,
        })
    }

    fn take_ack_rx(&mut self) -> io::Result<oneshot::Receiver<Result<(), String>>> {
        self.ack_rx
            .take()
            .ok_or_else(|| io::Error::other("pending outbound chunk missing ack receiver"))
    }
}

pub struct EnvelopeStream {
    outbound_tx: Sender<OutboundChunk>,
    inbound_rx: Receiver<InboundEvent>,
    inbound: InboundPlaintext,
    outbound: OutboundPlaintext,
    pending_chunk: Option<PendingChunk>,
    outstanding_acks: VecDeque<oneshot::Receiver<Result<(), String>>>,
    shutdown_started: bool,
}

impl EnvelopeStream {
    pub(crate) fn new(
        outbound_tx: Sender<OutboundChunk>,
        inbound_rx: Receiver<InboundEvent>,
    ) -> Self {
        Self {
            outbound_tx,
            inbound_rx,
            inbound: InboundPlaintext::new(),
            outbound: OutboundPlaintext::new(),
            pending_chunk: None,
            outstanding_acks: VecDeque::new(),
            shutdown_started: false,
        }
    }

    fn poll_send_pending(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let Some(mut chunk) = self.pending_chunk.take() else {
            return Poll::Ready(Ok(()));
        };

        match Pin::new(&mut self.outbound_tx).poll_ready(cx) {
            Poll::Ready(Ok(())) => {
                let outbound = match chunk.take_outbound() {
                    Ok(outbound) => outbound,
                    Err(error) => return Poll::Ready(Err(error)),
                };
                match Pin::new(&mut self.outbound_tx).start_send(outbound) {
                    Ok(()) => {
                        let ack_rx = match chunk.take_ack_rx() {
                            Ok(ack_rx) => ack_rx,
                            Err(error) => return Poll::Ready(Err(error)),
                        };
                        self.outstanding_acks.push_back(ack_rx);
                        Poll::Ready(Ok(()))
                    }
                    Err(_) => Poll::Ready(Err(actor_closed_io_error())),
                }
            }
            Poll::Ready(Err(_)) => Poll::Ready(Err(actor_closed_io_error())),
            Poll::Pending => {
                self.pending_chunk = Some(chunk);
                Poll::Pending
            }
        }
    }

    fn poll_send_chunk(&mut self, cx: &mut Context<'_>, chunk: Vec<u8>) -> Poll<io::Result<()>> {
        self.pending_chunk = Some(PendingChunk::new(chunk));
        self.poll_send_pending(cx)
    }

    fn poll_send_full_chunks(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        ready!(self.poll_send_pending(cx))?;
        while let Some(chunk) = self.outbound.take_full_chunk() {
            ready!(self.poll_send_chunk(cx, chunk))?;
        }
        Poll::Ready(Ok(()))
    }

    fn poll_outstanding_acks(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        while let Some(ack) = self.outstanding_acks.front_mut() {
            match Pin::new(ack).poll(cx) {
                Poll::Ready(Ok(Ok(()))) => {
                    self.outstanding_acks.pop_front();
                }
                Poll::Ready(Ok(Err(message))) => {
                    self.outstanding_acks.pop_front();
                    return Poll::Ready(Err(io::Error::other(message)));
                }
                Poll::Ready(Err(_)) => {
                    self.outstanding_acks.pop_front();
                    return Poll::Ready(Err(actor_closed_io_error()));
                }
                Poll::Pending => return Poll::Pending,
            }
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
            Poll::Ready(Ok(())) => self.poll_outstanding_acks(cx),
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use futures_channel::mpsc::channel;
    use futures_util::StreamExt;
    use tokio::io::AsyncWriteExt;
    use tokio::time::timeout;

    use super::*;

    #[tokio::test]
    async fn flush_waits_for_outbound_ack() {
        let (outbound_tx, mut outbound_rx) = channel(1);
        let (_inbound_tx, inbound_rx) = tokio::sync::mpsc::channel(1);
        let mut stream = EnvelopeStream::new(outbound_tx, inbound_rx);

        stream.write_all(b"hello").await.unwrap();
        let mut flush = Box::pin(stream.flush());
        assert!(
            timeout(Duration::from_millis(25), &mut flush)
                .await
                .is_err(),
            "flush returned before the MQTT actor acknowledged the chunk"
        );

        let chunk = outbound_rx.next().await.unwrap();
        assert_eq!(chunk.bytes, b"hello");
        chunk.ack.send(Ok(())).unwrap();

        timeout(Duration::from_secs(1), flush)
            .await
            .unwrap()
            .unwrap();
    }
}
