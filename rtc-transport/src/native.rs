use std::sync::Arc;
use std::time::Duration;

use bytes::BytesMut;
use futures_util::StreamExt;
use futures_util::future::Abortable;
use protocol::types::{MOBILE_RTC_CHANNEL_ID, MOBILE_RTC_CHANNEL_LABEL, MobileIceServer};
use tokio::sync::watch;
use webrtc::data_channel::{DataChannel, DataChannelEvent, RTCDataChannelInit};
use webrtc::peer_connection::{
    PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCConfigurationBuilder,
    RTCIceGatheringState, RTCIceServer, RTCIceTransportPolicy, RTCPeerConnectionState,
    RTCSessionDescription,
};
use webrtc::runtime::TokioRuntime;

use crate::{
    BUFFER_BYTES, CHUNK_BYTES, Error, RtcStream, StreamEndpoint, WINDOW_CHUNKS, failure,
    stream_pair,
};

struct Handler {
    gathered: watch::Sender<bool>,
    failure: watch::Sender<Option<String>>,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for Handler {
    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        tracing::debug!(?state, "mobile WebRTC ICE gathering changed");
        if state == RTCIceGatheringState::Complete {
            self.gathered.send_replace(true);
        }
    }

    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        tracing::info!(?state, "mobile WebRTC connection state changed");
        if matches!(
            state,
            RTCPeerConnectionState::Failed
                | RTCPeerConnectionState::Closed
                | RTCPeerConnectionState::Disconnected
        ) {
            self.failure
                .send_replace(Some(format!("WebRTC connection entered {state}")));
        }
    }
}

pub struct Peer {
    connection: Arc<dyn PeerConnection>,
    channel: Arc<dyn DataChannel>,
    gathered: watch::Receiver<bool>,
    failure: watch::Receiver<Option<String>>,
}

impl Peer {
    pub async fn new(servers: &[MobileIceServer]) -> Result<Self, Error> {
        if servers.is_empty() {
            return Err(failure("configuration", "TURN credentials are required"));
        }
        if servers.iter().any(|server| {
            server.urls.is_empty()
                || server.urls.iter().any(|url| {
                    !url.as_str().starts_with("turn:") || !url.as_str().ends_with("?transport=udp")
                })
        }) {
            return Err(failure(
                "configuration",
                "native TURN requires explicitly configured UDP relay URLs",
            ));
        }
        let ice_servers = servers
            .iter()
            .map(|server| RTCIceServer {
                urls: server.urls.iter().map(ToString::to_string).collect(),
                username: server.username.clone(),
                credential: server.credential.clone(),
            })
            .collect();
        let (gathered_tx, gathered) = watch::channel(false);
        let (failure_tx, failure_rx) = watch::channel(None);
        let connection = PeerConnectionBuilder::new()
            .with_configuration(
                RTCConfigurationBuilder::new()
                    .with_ice_servers(ice_servers)
                    .with_ice_transport_policy(RTCIceTransportPolicy::Relay)
                    .build(),
            )
            .with_handler(Arc::new(Handler {
                gathered: gathered_tx,
                failure: failure_tx,
            }))
            .with_runtime(Arc::new(TokioRuntime))
            .with_udp_addrs(vec!["0.0.0.0:0"])
            .with_data_channel_send_buffer_limit(BUFFER_BYTES)
            .build()
            .await
            .map_err(|error| failure("create peer", error))?;
        let connection: Arc<dyn PeerConnection> = Arc::new(connection);
        let options = RTCDataChannelInit {
            negotiated: Some(MOBILE_RTC_CHANNEL_ID),
            protocol: MOBILE_RTC_CHANNEL_LABEL.to_owned(),
            ..Default::default()
        };
        let channel = match connection
            .create_data_channel(MOBILE_RTC_CHANNEL_LABEL, Some(options))
            .await
        {
            Ok(channel) => channel,
            Err(error) => {
                if let Err(close_error) = connection.close().await {
                    tracing::warn!(%close_error, "failed to close uninitialized WebRTC peer");
                }
                return Err(failure("create data channel", error));
            }
        };
        Ok(Self {
            connection,
            channel,
            gathered,
            failure: failure_rx,
        })
    }

    pub async fn offer(&mut self) -> Result<String, Error> {
        let offer = self
            .connection
            .create_offer(None)
            .await
            .map_err(|error| failure("create offer", error))?;
        self.set_local(offer).await
    }

    pub async fn answer(&mut self, offer: String) -> Result<String, Error> {
        let offer =
            RTCSessionDescription::offer(offer).map_err(|error| failure("parse offer", error))?;
        self.connection
            .set_remote_description(offer)
            .await
            .map_err(|error| failure("apply offer", error))?;
        let answer = self
            .connection
            .create_answer(None)
            .await
            .map_err(|error| failure("create answer", error))?;
        self.set_local(answer).await
    }

    pub async fn set_answer(&self, answer: String) -> Result<(), Error> {
        let answer = RTCSessionDescription::answer(answer)
            .map_err(|error| failure("parse answer", error))?;
        self.connection
            .set_remote_description(answer)
            .await
            .map_err(|error| failure("apply answer", error))
    }

    async fn set_local(&mut self, description: RTCSessionDescription) -> Result<String, Error> {
        self.connection
            .set_local_description(description)
            .await
            .map_err(|error| failure("apply local description", error))?;
        tokio::time::timeout(
            Duration::from_secs(20),
            self.gathered.wait_for(|complete| *complete),
        )
        .await
        .map_err(|error| failure("gather TURN candidates", error))?
        .map_err(|error| failure("gather TURN candidates", error))?;
        let description = self
            .connection
            .local_description()
            .await
            .ok_or_else(|| failure("gather TURN candidates", "local description is missing"))?;
        if !description
            .sdp
            .lines()
            .any(|line| line.starts_with("a=candidate:") && line.contains(" typ relay"))
        {
            return Err(failure(
                "gather TURN candidates",
                "TURN did not allocate a relay candidate",
            ));
        }
        Ok(description.sdp)
    }

    pub async fn into_stream(self) -> Result<RtcStream, Error> {
        self.into_stream_for(Duration::from_secs(8 * 60 * 60)).await
    }

    pub(crate) async fn into_stream_for(mut self, lifetime: Duration) -> Result<RtcStream, Error> {
        let open = async {
            loop {
                tokio::select! {
                    result = self.failure.changed() => {
                        result.map_err(|error| failure("open data channel", error))?;
                        if let Some(message) = self.failure.borrow().clone() {
                            return Err(failure("open data channel", message));
                        }
                    }
                    event = self.channel.poll() => {
                        match event {
                            Some(DataChannelEvent::OnOpen) => return Ok(()),
                            Some(DataChannelEvent::OnClose | DataChannelEvent::OnError) | None =>
                                return Err(failure("open data channel", "channel closed before opening")),
                            Some(DataChannelEvent::OnMessage(_)) =>
                                return Err(failure("open data channel", "received data before channel opened")),
                            _ => {}
                        }
                    }
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(25), open)
            .await
            .map_err(|error| failure("open data channel", error))??;
        tracing::info!("mobile WebRTC data channel opened through TURN");
        let (stream, worker, registration) = stream_pair();
        let errors = worker.inbound.clone();
        tokio::spawn(async move {
            let run = async {
                tokio::select! {
                    result = self.run(worker) => result,
                    _ = tokio::time::sleep(lifetime) => Err(failure("credentials", "TURN credential renewal is required")),
                }
            };
            let result = Abortable::new(run, registration).await;
            if let Ok(Err(error)) = result {
                tracing::warn!(%error, "mobile WebRTC byte stream failed");
                if let Err(send_error) = errors.send(Err(error.to_string())).await {
                    tracing::debug!(%send_error, "WebRTC stream reader already closed");
                }
            }
        });
        Ok(stream)
    }

    async fn run(&mut self, mut worker: StreamEndpoint) -> Result<(), Error> {
        let (pending_tx, mut pending_rx) = tokio::sync::mpsc::channel(WINDOW_CHUNKS);
        let send = async {
            while let Some(outbound) = worker.outbound.next().await {
                let mut bytes = BytesMut::with_capacity(outbound.bytes.len() + 1);
                bytes.extend_from_slice(&[0]);
                bytes.extend_from_slice(&outbound.bytes);
                pending_tx
                    .send(outbound.accepted)
                    .await
                    .map_err(|error| failure("queue acknowledgement", error))?;
                self.channel
                    .send(bytes)
                    .await
                    .map_err(|error| failure("send data", error))?;
            }
            Ok(())
        };
        let receive = async {
            let mut pending_data = std::collections::VecDeque::new();
            loop {
                tokio::select! {
                    // A full data buffer must not block ACKs for the reverse direction.
                    permit = worker.inbound.reserve(), if !pending_data.is_empty() => {
                        let permit = permit.map_err(|error| failure("reserve received bytes", error))?;
                        let bytes = pending_data.pop_front().ok_or_else(|| failure("receive data", "pending chunk disappeared"))?;
                        self.channel.send(BytesMut::from(&[1][..])).await.map_err(|error| failure("acknowledge data", error))?;
                        permit.send(Ok(bytes));
                    }
                    event = self.channel.poll() => match event {
                    Some(DataChannelEvent::OnMessage(message)) => {
                        if message.is_string || message.data.len() > CHUNK_BYTES + 1 {
                            return Err(failure(
                                "receive data",
                                "expected a bounded binary Tyde chunk",
                            ));
                        }
                        match message.data.first() {
                            Some(0) if message.data.len() > 1 => {
                                if pending_data.len() == WINDOW_CHUNKS {
                                    return Err(failure("receive data", "peer exceeded the unacknowledged chunk window"));
                                }
                                pending_data.push_back(message.data[1..].to_vec());
                            }
                            Some(1) if message.data.len() == 1 => {
                                let ack = pending_rx.try_recv().map_err(|error| {
                                    failure("unexpected acknowledgement", error)
                                })?;
                                if ack.send(Ok(())).is_err() {
                                    return Ok(());
                                }
                            }
                            _ => {
                                return Err(failure(
                                    "receive data",
                                    "invalid Tyde data channel record",
                                ));
                            }
                        }
                    }
                    Some(DataChannelEvent::OnClose) | None => return Ok(()),
                    Some(DataChannelEvent::OnError) => {
                        return Err(failure("receive data", "data channel reported an error"));
                    }
                    _ => {}
                    }
                }
            }
        };
        tokio::select! {
            result = send => result,
            result = receive => result,
            result = self.failure.changed() => {
                result.map_err(|error| failure("connection state", error))?;
                match self.failure.borrow().as_ref() {
                    Some(message) => Err(failure("connection state", message)),
                    None => Err(failure("connection state", "connection state changed without a reason")),
                }
            }
        }
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        let connection = Arc::clone(&self.connection);
        let channel = Arc::clone(&self.channel);
        tokio::spawn(async move {
            let drain = async {
                while channel.outstanding_bytes().await? > 0 {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Ok::<(), webrtc::error::Error>(())
            };
            match tokio::time::timeout(Duration::from_secs(5), drain).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    tracing::warn!(%error, "WebRTC acknowledgements could not drain before close")
                }
                Err(error) => {
                    tracing::warn!(%error, "WebRTC acknowledgement drain timed out before close")
                }
            }
            if let Err(error) = connection.close().await {
                tracing::warn!(%error, "failed to close mobile WebRTC peer");
            }
        });
    }
}
