use crate::peer_connection::driver::PeerConnectionDriverEvent;
use crate::peer_connection::transports::{TcpReadResult, is_retryable_socket_recv_error};
use crate::runtime::{AsyncTcpListener, AsyncTcpStream, Runtime, Sender};
use bytes::BytesMut;
use futures::FutureExt;
use futures::future::BoxFuture;
use futures::stream::FuturesUnordered;
use log::{error, trace};
use rtc::ice::candidate::Candidate;
use rtc::peer_connection::transport::{
    CandidateConfig, CandidateHostConfig, RTCIceCandidate, RTCIceCandidateInit,
};
use rtc::shared::FourTuple;
use rtc::shared::error::{Error, Result};
use rtc::shared::tcp_framing::{TcpFrameDecoder, frame_packet};
use rtc::shared::{TaggedBytesMut, TransportContext, TransportProtocol};
use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

const TCP_READ_BUF_LEN: usize = 4096;

pub(crate) type TcpAcceptResult = (
    SocketAddr,
    io::Result<(Arc<dyn AsyncTcpStream>, SocketAddr)>,
);

enum FrameDecoder {
    Ice(TcpFrameDecoder),
    Turn(BytesMut),
}

impl FrameDecoder {
    fn extend_from_slice(&mut self, bytes: &[u8]) {
        match self {
            Self::Ice(decoder) => decoder.extend_from_slice(bytes),
            Self::Turn(buffer) => buffer.extend_from_slice(bytes),
        }
    }

    fn next_packet(&mut self) -> Result<Option<BytesMut>> {
        let buffer = match self {
            Self::Ice(decoder) => return Ok(decoder.next_packet().map(|packet| BytesMut::from(&packet[..]))),
            Self::Turn(buffer) => buffer,
        };
        if buffer.len() < 4 { return Ok(None); }
        let payload = u16::from_be_bytes([buffer[2], buffer[3]]) as usize;
        let (length, padded) = match buffer[0] >> 6 {
            0 if payload.is_multiple_of(4) => (20 + payload, 20 + payload),
            1 => (4 + payload, 4 + payload.div_ceil(4) * 4),
            _ => return Err(Error::Other("invalid TURN stream record header".to_owned())),
        };
        if buffer.len() < padded { return Ok(None); }
        if buffer[0] >> 6 == 0 && buffer[4..8] != [0x21, 0x12, 0xa4, 0x42] {
            return Err(Error::Other("invalid TURN STUN magic cookie".to_owned()));
        }
        let mut packet = buffer.split_to(padded);
        packet.truncate(length);
        Ok(Some(packet))
    }
}

pub(crate) struct RTCTcpTransport {
    listeners: HashMap<SocketAddr, Arc<dyn AsyncTcpListener>>,
    streams: HashMap<FourTuple, Arc<dyn AsyncTcpStream>>,
    decoders: HashMap<FourTuple, FrameDecoder>,
    pub(crate) accept_futures: FuturesUnordered<BoxFuture<'static, TcpAcceptResult>>,
    pub(crate) read_futures: FuturesUnordered<BoxFuture<'static, TcpReadResult>>,
}

impl RTCTcpTransport {
    pub(crate) fn new(tcp_listeners: HashMap<SocketAddr, Arc<dyn AsyncTcpListener>>) -> Self {
        let accept_futures = FuturesUnordered::new();
        for (local_addr, listener) in &tcp_listeners {
            let local_addr = *local_addr;
            let listener = listener.clone();
            accept_futures.push(
                async move {
                    match listener.accept().await {
                        Ok((stream, peer_addr)) => (local_addr, Ok((stream, peer_addr))),
                        Err(err) => (local_addr, Err(err)),
                    }
                }
                .boxed(),
            );
        }

        Self {
            listeners: tcp_listeners,
            streams: HashMap::new(),
            decoders: HashMap::new(),
            accept_futures,
            read_futures: FuturesUnordered::new(),
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.listeners.is_empty()
    }

    pub(crate) fn listener_count(&self) -> usize {
        self.listeners.len()
    }

    pub(crate) fn has_stream_for(&self, four_tuple: &FourTuple) -> bool {
        self.streams.contains_key(four_tuple)
    }

    fn find_stream(&self, four_tuple: &FourTuple) -> Option<Arc<dyn AsyncTcpStream>> {
        self.streams.get(four_tuple).cloned().or_else(|| {
            self.streams
                .values()
                .find(|stream| {
                    stream
                        .peer_addr()
                        .is_ok_and(|peer| peer == four_tuple.peer_addr)
                })
                .cloned()
        })
    }

    fn remove_stream(&mut self, four_tuple: &FourTuple) {
        self.streams.remove(four_tuple);
        self.decoders.remove(four_tuple);
    }

    pub(crate) fn write<'a>(
        &self,
        msg: &'a TaggedBytesMut,
    ) -> Pin<Box<dyn Future<Output = Result<usize>> + Send + 'a>> {
        let four_tuple = FourTuple::from(&msg.transport);
        let Some(stream) = self.find_stream(&four_tuple) else {
            trace!("No TCP stream found for {:?}", four_tuple);
            return Box::pin(async { Ok(0) });
        };

        let framed = if matches!(self.decoders.get(&four_tuple), Some(FrameDecoder::Turn(_))) {
            // RFC 8656: TURN has its own headers, and ChannelData is padded
            // to four bytes on a stream. It does not use ICE-TCP's prefix.
            let mut bytes = msg.message.to_vec();
            if bytes.first().is_some_and(|byte| byte >> 6 == 1) {
                bytes.resize(bytes.len().div_ceil(4) * 4, 0);
            }
            bytes
        } else {
            frame_packet(&msg.message)
        };
        let len = msg.message.len();
        Box::pin(async move {
            stream.write_all(&framed).await?;
            Ok(len)
        })
    }

    fn arm_accept(&mut self, local_addr: SocketAddr) {
        if let Some(listener) = self.listeners.get(&local_addr).cloned() {
            self.accept_futures.push(
                async move {
                    match listener.accept().await {
                        Ok((stream, peer_addr)) => (local_addr, Ok((stream, peer_addr))),
                        Err(err) => (local_addr, Err(err)),
                    }
                }
                .boxed(),
            );
        }
    }

    fn arm_read(&mut self, four_tuple: FourTuple, stream: Arc<dyn AsyncTcpStream>) {
        self.read_futures.push(
            async move {
                let mut buf = vec![0u8; TCP_READ_BUF_LEN];
                match stream.read(&mut buf).await {
                    Ok(n) => TcpReadResult::Packet { four_tuple, n, buf },
                    Err(err) => TcpReadResult::Error {
                        four_tuple,
                        err,
                        buf,
                    },
                }
            }
            .boxed(),
        );
    }

    pub(crate) fn register_stream(
        &mut self,
        four_tuple: FourTuple,
        stream: Arc<dyn AsyncTcpStream>,
    ) {
        self.streams.insert(four_tuple, stream.clone());
        self.decoders.insert(four_tuple, FrameDecoder::Ice(TcpFrameDecoder::new()));
        self.arm_read(four_tuple, stream);
    }

    pub(crate) fn register_turn_stream(&mut self, four_tuple: FourTuple, stream: Arc<dyn AsyncTcpStream>) {
        self.streams.insert(four_tuple, stream.clone());
        self.decoders.insert(four_tuple, FrameDecoder::Turn(BytesMut::new()));
        self.arm_read(four_tuple, stream);
    }

    pub(crate) fn on_accept(
        &mut self,
        local_addr: SocketAddr,
        res: io::Result<(Arc<dyn AsyncTcpStream>, SocketAddr)>,
    ) -> Option<FourTuple> {
        let accepted = match res {
            Ok((stream, peer_addr)) => Some((stream, peer_addr)),
            Err(err) => {
                error!("TCP accept error: {}", err);
                None
            }
        };

        self.arm_accept(local_addr);

        let (stream, peer_addr) = accepted?;

        let stream_local_addr = stream.local_addr().unwrap_or(local_addr);
        let four_tuple = FourTuple {
            local_addr: stream_local_addr,
            peer_addr,
        };
        trace!(
            "Accepted TCP stream on {} from {}",
            stream_local_addr, peer_addr
        );
        self.register_stream(four_tuple, stream);
        Some(four_tuple)
    }

    pub(crate) fn on_read(&mut self, res: TcpReadResult) -> Result<Vec<TaggedBytesMut>> {
        let mut out = Vec::new();
        match res {
            TcpReadResult::Packet { four_tuple, n, buf } => {
                if n == 0 {
                    if matches!(self.decoders.get(&four_tuple), Some(FrameDecoder::Turn(_))) {
                        self.remove_stream(&four_tuple);
                        return Err(Error::Other("TURN TLS connection closed".to_owned()));
                    }
                    trace!("TCP connection EOF for {:?}", four_tuple);
                    self.remove_stream(&four_tuple);
                } else {
                    if let Some(decoder) = self.decoders.get_mut(&four_tuple) {
                        decoder.extend_from_slice(&buf[..n]);
                        while let Some(packet) = decoder.next_packet()? {
                            out.push(TaggedBytesMut {
                                now: Instant::now(),
                                transport: TransportContext {
                                    local_addr: four_tuple.local_addr,
                                    peer_addr: four_tuple.peer_addr,
                                    ecn: None,
                                    transport_protocol: TransportProtocol::TCP,
                                },
                                message: BytesMut::from(&packet[..]),
                            });
                        }
                    }
                    if let Some(stream) = self.streams.get(&four_tuple).cloned() {
                        self.arm_read(four_tuple, stream);
                    }
                }
            }
            TcpReadResult::Error {
                four_tuple,
                err,
                buf: _,
            } => {
                if matches!(self.decoders.get(&four_tuple), Some(FrameDecoder::Turn(_))) {
                    self.remove_stream(&four_tuple);
                    return Err(Error::Other(format!("TURN TLS read failed: {err}")));
                }
                if is_retryable_socket_recv_error(&err) {
                    trace!("Transient TCP read error on {:?}: {}", four_tuple, err);
                    if let Some(stream) = self.streams.get(&four_tuple).cloned() {
                        self.arm_read(four_tuple, stream);
                    }
                } else {
                    error!("TCP read error on {:?}: {}", four_tuple, err);
                    self.remove_stream(&four_tuple);
                }
            }
        }
        Ok(out)
    }

    pub(crate) fn gather_candidates(&self) -> Vec<RTCIceCandidateInit> {
        let mut candidates = Vec::new();
        for local_addr in self.listeners.keys() {
            // Gather passive TCP candidate
            let passive_config = CandidateHostConfig {
                base_config: CandidateConfig {
                    network: "tcp".to_owned(),
                    address: local_addr.ip().to_string(),
                    port: local_addr.port(),
                    component: 1,
                    ..Default::default()
                },
                tcp_type: rtc::ice::tcp_type::TcpType::Passive,
            };
            if let Ok(candidate) = passive_config.new_candidate_host()
                && let Ok(candidate_init) = RTCIceCandidate::from(&candidate).to_json()
            {
                candidates.push(candidate_init);
            }

            // Gather active TCP candidate
            let active_config = CandidateHostConfig {
                base_config: CandidateConfig {
                    network: "tcp".to_owned(),
                    address: local_addr.ip().to_string(),
                    port: 9, // Discard port placeholder for active candidates
                    component: 1,
                    ..Default::default()
                },
                tcp_type: rtc::ice::tcp_type::TcpType::Active,
            };
            if let Ok(candidate) = active_config.new_candidate_host()
                && let Ok(candidate_init) = RTCIceCandidate::from(&candidate).to_json()
            {
                candidates.push(candidate_init);
            }
        }
        candidates
    }

    pub(crate) fn connect(
        candidate: &Candidate,
        runtime: Arc<dyn Runtime>,
        tx: Sender<PeerConnectionDriverEvent>,
    ) {
        if candidate.network_type().is_tcp()
            && candidate.tcp_type() == rtc::ice::tcp_type::TcpType::Passive
            && let Ok(ip) = candidate.address().parse::<IpAddr>()
        {
            let remote_addr = SocketAddr::new(ip, candidate.port());
            let runtime_clone = runtime.clone();
            runtime.spawn(Box::pin(async move {
                trace!("Initiating TCP connect to {:?}", remote_addr);
                match runtime_clone.connect_tcp(remote_addr).await {
                    Ok(stream) => {
                        let local_addr = stream
                            .local_addr()
                            .unwrap_or_else(|_| "0.0.0.0:0".parse().unwrap());
                        let peer_addr = stream.peer_addr().unwrap_or(remote_addr);
                        let four_tuple = FourTuple {
                            local_addr,
                            peer_addr,
                        };
                        let _ = tx
                            .send(PeerConnectionDriverEvent::IncomingTcpStream(
                                four_tuple, stream,
                            ))
                            .await;
                    }
                    Err(err) => {
                        error!("Failed to connect TCP to {:?}: {}", remote_addr, err);
                    }
                }
            }));
        }
    }
}
