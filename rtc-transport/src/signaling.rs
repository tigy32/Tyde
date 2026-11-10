use protocol::types::{
    MOBILE_RTC_PROTOCOL_VERSION, MobilePeerRole, MobileRtcCredentials, MobileRtcSessionId,
    MobileSdpKind, MobileSignalCommand, MobileSignalEvent,
};
use std::time::Duration;

use crate::{
    ConnectStage, Error, Peer, RtcStream, authenticate_description, failure, verify_description,
};

use tyde_time::timeout;

pub async fn connect(
    credentials: MobileRtcCredentials,
    pairing_key: &[u8; 32],
    progress: impl FnMut(ConnectStage),
) -> Result<RtcStream, Error> {
    connect_with(credentials, pairing_key, progress, |servers| async move {
        Peer::new(&servers).await
    })
    .await
}

#[cfg(not(target_arch = "wasm32"))]
pub async fn connect_with_tls_roots(
    credentials: MobileRtcCredentials,
    pairing_key: &[u8; 32],
    progress: impl FnMut(ConnectStage),
    roots: tokio_rustls::rustls::RootCertStore,
) -> Result<RtcStream, Error> {
    connect_with(credentials, pairing_key, progress, |servers| async move {
        Peer::with_tls_roots(&servers, roots).await
    })
    .await
}

async fn connect_with<F: std::future::Future<Output = Result<Peer, Error>>>(
    credentials: MobileRtcCredentials,
    pairing_key: &[u8; 32],
    mut progress: impl FnMut(ConnectStage),
    new_peer: impl FnOnce(Vec<protocol::MobileIceServer>) -> F,
) -> Result<RtcStream, Error> {
    if credentials.protocol_version != MOBILE_RTC_PROTOCOL_VERSION {
        return Err(failure(
            "configuration",
            "unsupported WebRTC protocol version",
        ));
    }
    let url = url::Url::parse(credentials.signaling_url.as_str())
        .map_err(|error| failure("signaling URL", error))?;
    let local = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    if !(url.scheme() == "https" || url.scheme() == "http" && local)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.query().is_some()
    {
        return Err(failure(
            "signaling URL",
            "expected HTTPS without embedded credentials",
        ));
    }
    let builder = reqwest::Client::builder();
    #[cfg(not(target_arch = "wasm32"))]
    let builder = builder.redirect(reqwest::redirect::Policy::none());
    let signal = Signaling {
        client: builder
            .build()
            .map_err(|error| failure("signaling client", error))?,
        credentials,
    };
    let remaining = || -> Result<Duration, Error> {
        #[cfg(target_arch = "wasm32")]
        let now = js_sys::Date::now() as u64;
        #[cfg(not(target_arch = "wasm32"))]
        let now = u64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|error| failure("credential clock", error))?
                .as_millis(),
        )
        .map_err(|error| failure("credential clock", error))?;
        let remaining = signal
            .credentials
            .expires_at_ms
            .checked_sub(now.saturating_add(60_000))
            .filter(|remaining| *remaining > 0)
            .ok_or_else(|| {
                failure(
                    "credentials",
                    "TURN credentials have reached their renewal deadline",
                )
            })?;
        Ok(Duration::from_millis(remaining))
    };
    let deadline = remaining()?;
    let mut stage = ConnectStage::RelayAllocation;
    let mut advance = |next| {
        stage = next;
        progress(next);
    };
    let negotiation = async {
        let peer = match signal.credentials.role {
            MobilePeerRole::Mobile => {
                advance(ConnectStage::RelayAllocation);
                let mut peer = new_peer(signal.credentials.ice_servers.clone()).await?;
                let session_id = MobileRtcSessionId(uuid::Uuid::new_v4().to_string());
                let offer = peer.offer().await?;
                advance(ConnectStage::PublishOffer);
                advance(ConnectStage::AwaitHostAnswer);
                let description = signal
                    .exchange(MobileSignalCommand::Publish {
                        description: authenticate_description(
                            session_id.clone(),
                            MobileSdpKind::Offer,
                            offer,
                            pairing_key,
                        )?,
                    })
                    .await?;
                let description = require_description(description)?;
                verify_description(
                    &description,
                    &session_id,
                    MobileSdpKind::Answer,
                    pairing_key,
                )?;
                peer.set_answer(description.sdp).await?;
                peer
            }
            MobilePeerRole::Host => {
                advance(ConnectStage::AwaitMobileOffer);
                let mut socket = HostSocket::open(&signal.credentials).await?;
                let description = require_description(socket.receive().await?)?;
                let session_id = description.session_id.clone();
                verify_description(&description, &session_id, MobileSdpKind::Offer, pairing_key)?;
                advance(ConnectStage::CreateAnswer);
                let mut peer = new_peer(signal.credentials.ice_servers.clone()).await?;
                let answer = peer.answer(description.sdp).await?;
                advance(ConnectStage::PublishAnswer);
                socket
                    .publish(authenticate_description(
                        session_id,
                        MobileSdpKind::Answer,
                        answer,
                        pairing_key,
                    )?)
                    .await?;
                peer
            }
        };
        advance(ConnectStage::OpenChannel);
        peer.into_stream_for(remaining()?).await
    };
    let duration = match signal.credentials.role {
        MobilePeerRole::Mobile => Duration::from_secs(60),
        MobilePeerRole::Host => deadline,
    };
    timeout(duration, negotiation)
        .await
        .map_err(|_| Error::NegotiationTimeout { stage })?
}

struct Signaling {
    client: reqwest::Client,
    credentials: MobileRtcCredentials,
}

impl Signaling {
    async fn exchange(&self, command: MobileSignalCommand) -> Result<MobileSignalEvent, Error> {
        let response = self
            .client
            .post(self.credentials.signaling_url.as_str())
            .bearer_auth(&self.credentials.signaling_token)
            .json(&command)
            .send()
            .await
            .map_err(|error| failure("signaling request", error))?;
        if !response.status().is_success() {
            return Err(failure(
                "signaling",
                format!("service refused request (HTTP {})", response.status()),
            ));
        }
        if response
            .content_length()
            .is_some_and(|length| length > 32 * 1024)
        {
            return Err(failure("signaling", "response exceeds 32 KiB"));
        }
        let body = response
            .bytes()
            .await
            .map_err(|error| failure("signaling response", error))?;
        if body.len() > 32 * 1024 {
            return Err(failure("signaling", "response exceeds 32 KiB"));
        }
        serde_json::from_slice(&body).map_err(|error| failure("signaling response", error))
    }
}

fn require_description(event: MobileSignalEvent) -> Result<protocol::MobileRtcDescription, Error> {
    match event {
        MobileSignalEvent::Description { description } => Ok(description),
        MobileSignalEvent::Failed { message } => Err(failure("signaling", message)),
        MobileSignalEvent::Waiting => Err(failure(
            "signaling",
            "unexpected acknowledgement while awaiting description",
        )),
    }
}

#[cfg(not(target_arch = "wasm32"))]
struct HostSocket {
    socket: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    ping: tokio::time::Interval,
}

#[cfg(not(target_arch = "wasm32"))]
impl HostSocket {
    async fn open(credentials: &MobileRtcCredentials) -> Result<Self, Error> {
        use tokio_tungstenite::tungstenite::{client::IntoClientRequest, http::HeaderValue};
        let mut url = url::Url::parse(credentials.signaling_url.as_str())
            .map_err(|error| failure("signaling URL", error))?;
        let scheme = if url.scheme() == "https" { "wss" } else { "ws" };
        url.set_scheme(scheme)
            .map_err(|_| failure("signaling URL", "invalid WebSocket scheme"))?;
        let mut request = url
            .as_str()
            .into_client_request()
            .map_err(|error| failure("signaling socket", error))?;
        request.headers_mut().insert(
            "Authorization",
            HeaderValue::from_str(&format!("Bearer {}", credentials.signaling_token))
                .map_err(|_| failure("signaling authorization", "invalid token header"))?,
        );
        let config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
            .max_message_size(Some(32 * 1024))
            .max_frame_size(Some(32 * 1024));
        let (socket, _) = tokio_tungstenite::connect_async_with_config(request, Some(config), true)
            .await
            .map_err(|error| match error {
                tokio_tungstenite::tungstenite::Error::Http(response) => failure(
                    "signaling socket",
                    format!("service refused WebSocket upgrade (HTTP {}); check service compatibility and credentials", response.status()),
                ),
                _ => failure("signaling socket", "WebSocket upgrade failed"),
            })?;
        // Protocol pings do not invoke the service's billed application handler.
        const PING_INTERVAL: Duration = Duration::from_secs(25);
        let mut ping =
            tokio::time::interval_at(tokio::time::Instant::now() + PING_INTERVAL, PING_INTERVAL);
        ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tracing::debug!("host signaling socket connected");
        Ok(Self { socket, ping })
    }

    async fn receive(&mut self) -> Result<MobileSignalEvent, Error> {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;
        loop {
            tokio::select! {
                _ = self.ping.tick() => self.socket.send(Message::Ping(Vec::new().into())).await
                    .map_err(|_| failure("signaling socket", "ping failed"))?,
                frame = self.socket.next() => match frame {
                    Some(Ok(Message::Text(text))) => return serde_json::from_str(&text)
                        .map_err(|error| failure("signaling socket response", error)),
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => {
                        self.socket.flush().await.map_err(|_| failure("signaling socket", "control frame flush failed"))?;
                    }
                    Some(Ok(Message::Close(_))) | None => return Err(failure("signaling socket", "connection closed")),
                    Some(Err(_)) => return Err(failure("signaling socket", "connection failed")),
                    Some(Ok(_)) => return Err(failure("signaling socket", "expected JSON text frame")),
                }
            }
        }
    }

    async fn publish(&mut self, description: protocol::MobileRtcDescription) -> Result<(), Error> {
        use futures_util::SinkExt;
        use tokio_tungstenite::tungstenite::Message;
        let text = serde_json::to_string(&MobileSignalCommand::Publish { description })
            .map_err(|error| failure("signaling answer", error))?;
        self.socket
            .send(Message::Text(text.into()))
            .await
            .map_err(|_| failure("signaling socket", "answer send failed"))?;
        match self.receive().await? {
            MobileSignalEvent::Waiting => Ok(()),
            MobileSignalEvent::Failed { message } => Err(failure("signaling", message)),
            MobileSignalEvent::Description { .. } => Err(failure(
                "signaling",
                "unexpected description while awaiting answer acknowledgement",
            )),
        }
    }
}

#[cfg(target_arch = "wasm32")]
struct HostSocket;

#[cfg(target_arch = "wasm32")]
impl HostSocket {
    async fn open(_: &MobileRtcCredentials) -> Result<Self, Error> {
        Err(failure(
            "configuration",
            "browser cannot act as a signaling host",
        ))
    }
    async fn receive(&mut self) -> Result<MobileSignalEvent, Error> {
        Err(failure(
            "configuration",
            "browser cannot act as a signaling host",
        ))
    }
    async fn publish(&mut self, _: protocol::MobileRtcDescription) -> Result<(), Error> {
        Err(failure(
            "configuration",
            "browser cannot act as a signaling host",
        ))
    }
}
