use protocol::types::{
    MOBILE_RTC_PROTOCOL_VERSION, MobilePeerRole, MobileRtcCredentials, MobileRtcSessionId,
    MobileSdpKind, MobileSignalCommand, MobileSignalEvent,
};
use std::time::Duration;

#[cfg(not(target_arch = "wasm32"))]
use {
    futures_util::future::Either,
    protocol::MobileRtcDescription,
    std::{
        convert::Infallible,
        future::{Future, pending},
        pin::{Pin, pin},
    },
};

use crate::{
    ConnectStage, Error, Peer, RtcStream, authenticate_description, failure, verify_description,
};

use tyde_time::timeout;

/// The service answers this exact frame beside its signaling object. Edge
/// protocol pings can keep succeeding after the edge-to-object leg dies.
pub const SIGNALING_HEARTBEAT: &str = "tyde-signaling-heartbeat";
pub const SIGNALING_HEARTBEAT_ACK: &str = "tyde-signaling-heartbeat-ack";
pub const DEFAULT_SIGNALING_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);

pub async fn connect(
    credentials: MobileRtcCredentials,
    pairing_key: &[u8; 32],
    progress: impl FnMut(ConnectStage),
) -> Result<RtcStream, Error> {
    connect_with_signaling_heartbeat(
        credentials,
        pairing_key,
        progress,
        DEFAULT_SIGNALING_HEARTBEAT_INTERVAL,
    )
    .await
}

/// A host listener fails when a heartbeat is unanswered by the next one.
pub async fn connect_with_signaling_heartbeat(
    credentials: MobileRtcCredentials,
    pairing_key: &[u8; 32],
    progress: impl FnMut(ConnectStage),
    heartbeat: Duration,
) -> Result<RtcStream, Error> {
    connect_with(
        credentials,
        pairing_key,
        progress,
        heartbeat,
        |servers| async move { Peer::new(&servers).await },
    )
    .await
}

#[cfg(not(target_arch = "wasm32"))]
pub async fn connect_with_tls_roots(
    credentials: MobileRtcCredentials,
    pairing_key: &[u8; 32],
    progress: impl FnMut(ConnectStage),
    roots: tokio_rustls::rustls::RootCertStore,
) -> Result<RtcStream, Error> {
    connect_with(
        credentials,
        pairing_key,
        progress,
        DEFAULT_SIGNALING_HEARTBEAT_INTERVAL,
        |servers| {
            let roots = roots.clone();
            async move { Peer::with_tls_roots(&servers, roots).await }
        },
    )
    .await
}

async fn connect_with<F: std::future::Future<Output = Result<Peer, Error>>>(
    credentials: MobileRtcCredentials,
    pairing_key: &[u8; 32],
    mut progress: impl FnMut(ConnectStage),
    heartbeat: Duration,
    new_peer: impl Fn(Vec<protocol::MobileIceServer>) -> F,
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
    let mut stage = ConnectStage::AwaitMobileOffer;
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
                return accept_host(
                    &signal.credentials,
                    pairing_key,
                    advance,
                    heartbeat,
                    || new_peer(signal.credentials.ice_servers.clone()),
                    remaining,
                )
                .await;
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
async fn accept_host<F: Future<Output = Result<Peer, Error>>>(
    credentials: &MobileRtcCredentials,
    pairing_key: &[u8; 32],
    mut advance: impl FnMut(ConnectStage),
    heartbeat: Duration,
    new_peer: impl Fn() -> F,
    remaining: impl Fn() -> Result<Duration, Error>,
) -> Result<RtcStream, Error> {
    let mut socket = HostSocket::open(credentials, heartbeat).await?;
    advance(ConnectStage::AwaitMobileOffer);
    let mut offer = match socket
        .offer_or(pairing_key, pending::<Infallible>())
        .await?
    {
        Either::Left(offer) => offer,
        Either::Right(never) => match never {},
    };
    loop {
        advance(ConnectStage::CreateAnswer);
        let mut peer = new_peer().await?;
        let answer = match socket.offer_or(pairing_key, peer.answer(offer.sdp)).await? {
            Either::Left(replacement) => {
                tracing::debug!("fresh mobile offer replaced the offer being answered");
                offer = replacement;
                continue;
            }
            Either::Right(result) => result?,
        };
        advance(ConnectStage::PublishAnswer);
        socket
            .send_answer(authenticate_description(
                offer.session_id,
                MobileSdpKind::Answer,
                answer,
                pairing_key,
            )?)
            .await?;
        // A new offer can arrive before the old answer's untagged ACK.
        // Consume that ACK before sending another answer on this socket.
        if let Some(replacement) = socket.answer_outcome(pairing_key).await? {
            tracing::debug!("fresh mobile offer replaced the published answer");
            offer = replacement;
            continue;
        }
        advance(ConnectStage::OpenChannel);
        match socket
            .offer_or(pairing_key, peer.into_stream_for(remaining()?))
            .await?
        {
            Either::Left(replacement) => {
                tracing::debug!("fresh mobile offer replaced a pending data channel");
                offer = replacement;
            }
            Either::Right(result) => return result,
        }
    }
}

#[cfg(target_arch = "wasm32")]
async fn accept_host<F: std::future::Future<Output = Result<Peer, Error>>>(
    _: &MobileRtcCredentials,
    _: &[u8; 32],
    _: impl FnMut(ConnectStage),
    _: Duration,
    _: impl Fn() -> F,
    _: impl Fn() -> Result<Duration, Error>,
) -> Result<RtcStream, Error> {
    Err(failure(
        "configuration",
        "browser cannot act as a signaling host",
    ))
}

#[cfg(not(target_arch = "wasm32"))]
fn verify_offer(
    description: MobileRtcDescription,
    pairing_key: &[u8; 32],
) -> Result<MobileRtcDescription, Error> {
    verify_description(
        &description,
        &description.session_id,
        MobileSdpKind::Offer,
        pairing_key,
    )?;
    Ok(description)
}

#[cfg(not(target_arch = "wasm32"))]
struct HostSocket {
    socket: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    heartbeat: tokio::time::Interval,
    heartbeat_period: Duration,
    awaiting_ack: bool,
}

#[cfg(not(target_arch = "wasm32"))]
impl HostSocket {
    async fn open(credentials: &MobileRtcCredentials, period: Duration) -> Result<Self, Error> {
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
                error => failure("signaling socket", format!("WebSocket upgrade failed: {error}")),
            })?;
        let mut heartbeat = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tracing::debug!("host signaling socket connected");
        Ok(Self {
            socket,
            heartbeat,
            heartbeat_period: period,
            awaiting_ack: false,
        })
    }

    async fn receive_or<F: Future>(
        &mut self,
        mut work: Pin<&mut F>,
    ) -> Result<Either<MobileSignalEvent, F::Output>, Error> {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;
        loop {
            tokio::select! {
                // A buffered acknowledgement must win over an overdue tick.
                biased;
                frame = self.socket.next() => match frame {
                    Some(Ok(Message::Text(text))) if text.as_str() == SIGNALING_HEARTBEAT_ACK => {
                        self.awaiting_ack = false;
                    }
                    Some(Ok(Message::Text(text))) => return serde_json::from_str(&text).map(Either::Left)
                        .map_err(|error| failure("signaling socket response", error)),
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => {
                        self.socket.flush().await.map_err(|_| failure("signaling socket", "control frame flush failed"))?;
                    }
                    Some(Ok(Message::Close(_))) | None => return Err(failure("signaling socket", "connection closed")),
                    Some(Err(error)) => return Err(failure("signaling socket", format!("connection failed: {error}"))),
                    Some(Ok(_)) => return Err(failure("signaling socket", "expected JSON text frame")),
                },
                _ = self.heartbeat.tick() => {
                    if self.awaiting_ack {
                        return Err(failure(
                            "signaling socket",
                            format!("service heartbeat unanswered for {:?}", self.heartbeat_period),
                        ));
                    }
                    self.socket.send(Message::Text(SIGNALING_HEARTBEAT.into())).await
                        .map_err(|_| failure("signaling socket", "heartbeat send failed"))?;
                    self.awaiting_ack = true;
                }
                result = &mut work => return Ok(Either::Right(result)),
            }
        }
    }

    async fn offer_or<F: Future>(
        &mut self,
        pairing_key: &[u8; 32],
        work: F,
    ) -> Result<Either<MobileRtcDescription, F::Output>, Error> {
        match self.receive_or(pin!(work)).await? {
            Either::Left(event) => {
                verify_offer(require_description(event)?, pairing_key).map(Either::Left)
            }
            Either::Right(result) => Ok(Either::Right(result)),
        }
    }

    async fn send_answer(&mut self, description: MobileRtcDescription) -> Result<(), Error> {
        use futures_util::SinkExt;
        use tokio_tungstenite::tungstenite::Message;
        let text = serde_json::to_string(&MobileSignalCommand::Publish { description })
            .map_err(|error| failure("signaling answer", error))?;
        self.socket
            .send(Message::Text(text.into()))
            .await
            .map_err(|_| failure("signaling socket", "answer send failed"))
    }

    async fn answer_outcome(
        &mut self,
        pairing_key: &[u8; 32],
    ) -> Result<Option<MobileRtcDescription>, Error> {
        let mut next = None;
        loop {
            match self.receive_or(pin!(pending::<Infallible>())).await? {
                Either::Left(MobileSignalEvent::Waiting) => return Ok(next),
                Either::Left(MobileSignalEvent::Failed { message }) => {
                    // An authenticated replacement has already taken over the
                    // service's room, so an answer for the old room can fail.
                    return next.map(Some).ok_or_else(|| failure("signaling", message));
                }
                Either::Left(MobileSignalEvent::Description { description }) => {
                    next = Some(verify_offer(description, pairing_key)?);
                }
                Either::Right(never) => match never {},
            }
        }
    }
}
