use crate::{
    dots_mcp::DotsMcpServer,
    host::HostHandle,
    store::permissions::{atomic_write_owner_only, enforce_owner_only_file},
};
use futures_util::{SinkExt, StreamExt};
use protocol::{DotsCommandPayload, DotsConnectionStatus, DotsRelayFrame, DotsStatePayload};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{path::PathBuf, time::Duration};
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Error as WebSocketError, Message, client::IntoClientRequest},
};
use tokio_util::sync::CancellationToken;

pub(crate) const SERVICE_ORIGIN: &str = "https://mcp.tycode.dev";
const HEARTBEAT: Duration = Duration::from_secs(15);
const INITIAL_RECONNECT_DELAY: Duration = Duration::from_secs(1);
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub(crate) struct DotsHandle {
    tx: mpsc::UnboundedSender<RuntimeCommand>,
    pub state: watch::Receiver<DotsStatePayload>,
}

pub(crate) enum RuntimeCommand {
    Configure(bool),
    User(DotsCommandPayload),
}

pub(crate) struct ConnectionConfig {
    pub path: PathBuf,
    pub origin: url::Url,
    pub configuration_gate: Option<watch::Receiver<bool>>,
    pub heartbeat_ticks: Option<std::sync::Arc<tokio::sync::Semaphore>>,
}

async fn receive_command(
    commands: &mut mpsc::UnboundedReceiver<RuntimeCommand>,
    pending: &mut Option<RuntimeCommand>,
    gate: &mut Option<watch::Receiver<bool>>,
) -> Option<RuntimeCommand> {
    let Some(gate) = gate else {
        return commands.recv().await;
    };
    if pending.is_none() {
        // The connection may fail while the fixture holds a command.
        *pending = Some(commands.recv().await?);
    }
    while !*gate.borrow_and_update() {
        if gate.changed().await.is_err() {
            return None;
        }
    }
    pending.take()
}

impl DotsHandle {
    pub(crate) fn new() -> (
        Self,
        mpsc::UnboundedReceiver<RuntimeCommand>,
        watch::Sender<DotsStatePayload>,
    ) {
        let (tx, rx) = mpsc::unbounded_channel();
        let (state_tx, state) = watch::channel(DotsStatePayload::default());
        (Self { tx, state }, rx, state_tx)
    }
    pub(crate) fn configure(&self, enabled: bool) -> Result<(), String> {
        self.tx
            .send(RuntimeCommand::Configure(enabled))
            .map_err(|_| "Dots connection manager stopped".into())
    }
    pub(crate) fn command(&self, command: DotsCommandPayload) -> Result<(), String> {
        self.tx
            .send(RuntimeCommand::User(command))
            .map_err(|_| "Dots connection manager stopped".into())
    }
}

#[derive(Serialize, Deserialize)]
struct Credentials {
    host_id: String,
    host_key: String,
}

fn random() -> Result<String, String> {
    use aes_gcm::aead::{OsRng, rand_core::RngCore};
    let mut bytes = [0_u8; 32];
    OsRng
        .try_fill_bytes(&mut bytes)
        .map_err(|_| "Cannot create a secure Dots credential")?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn credentials(path: &std::path::Path) -> Result<Credentials, String> {
    match std::fs::read(path) {
        Ok(bytes) => {
            enforce_owner_only_file(path)?;
            serde_json::from_slice(&bytes).map_err(|_| "Cannot decode Dots host credentials".into())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let value = Credentials {
                host_id: random()?,
                host_key: random()?,
            };
            let bytes =
                serde_json::to_vec(&value).map_err(|_| "Cannot encode Dots host credentials")?;
            atomic_write_owner_only(path, &bytes)?;
            Ok(value)
        }
        Err(_) => Err("Cannot read Dots host credentials".into()),
    }
}

async fn publish(
    host: &HostHandle,
    state: &watch::Sender<DotsStatePayload>,
    value: DotsStatePayload,
) {
    if *state.borrow() == value {
        return;
    }
    state.send_replace(value.clone());
    host.publish_dots_state(value).await;
}

pub(crate) fn start(
    host: HostHandle,
    mut commands: mpsc::UnboundedReceiver<RuntimeCommand>,
    state: watch::Sender<DotsStatePayload>,
    config: ConnectionConfig,
    stopped: CancellationToken,
    mut complete: watch::Receiver<Option<Result<(), String>>>,
) -> Result<(), String> {
    let ConnectionConfig {
        path,
        origin,
        mut configuration_gate,
        heartbeat_ticks,
    } = config;
    let worker = async move {
        let mut enabled = match host.read_settings().await {
            Ok(settings) => settings.chatgpt_dots_enabled,
            Err(_) => {
                publish(
                    &host,
                    &state,
                    DotsStatePayload {
                        status: DotsConnectionStatus::Failed {
                            reason: "Cannot read Dots settings".into(),
                        },
                        ..Default::default()
                    },
                )
                .await;
                return;
            }
        };
        let mut connection_allowed = true;
        let mut pending = None;
        loop {
            let command = if enabled && connection_allowed {
                tokio::select! {
                    _ = stopped.cancelled() => break,
                    command = receive_command(&mut commands, &mut pending, &mut configuration_gate) => command,
                    () = maintain_connection(&host, &state, &path, &origin, heartbeat_ticks.as_deref()) => {
                        connection_allowed = false;
                        continue;
                    }
                }
            } else {
                tokio::select! {
                    _ = stopped.cancelled() => break,
                    command = receive_command(&mut commands, &mut pending, &mut configuration_gate) => command,
                }
            };
            match command {
                Some(RuntimeCommand::Configure(value)) => {
                    enabled = value;
                    connection_allowed = true;
                    if !enabled {
                        publish(&host, &state, DotsStatePayload::default()).await;
                    }
                }
                Some(RuntimeCommand::User(DotsCommandPayload::Retry)) => connection_allowed = true,
                Some(RuntimeCommand::User(DotsCommandPayload::Disconnect)) => {
                    match disconnect(&path, &origin).await {
                        Ok(()) => {
                            connection_allowed = true;
                            if !enabled {
                                publish(&host, &state, DotsStatePayload::default()).await;
                            }
                        }
                        Err(reason) => {
                            connection_allowed = false;
                            report_failure(&host, &state, reason).await;
                        }
                    }
                }
                None => break,
            }
        }
    };
    match tokio::runtime::Handle::try_current() {
        Ok(runtime) => {
            runtime.spawn(worker);
        }
        Err(_) => {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|_| "Cannot initialize Dots connection runtime")?;
            std::thread::Builder::new()
                .name("tyde-dots".into())
                .spawn(move || {
                    runtime.block_on(async move {
                        worker.await;
                        // Agents launched here must remain alive through the host's graceful shutdown.
                        while !matches!(*complete.borrow_and_update(), Some(Ok(()))) {
                            if complete.changed().await.is_err() {
                                break;
                            }
                        }
                    });
                })
                .map_err(|_| "Cannot start Dots connection runtime")?;
        }
    }
    Ok(())
}

struct SessionFailure {
    reason: String,
    reconnect: bool,
}

impl SessionFailure {
    fn transport(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            reconnect: true,
        }
    }

    fn websocket(context: &str, error: WebSocketError) -> Self {
        // Raw WebSocket errors may include remote close text, headers or requested URLs.
        let (detail, reconnect) = match error {
            WebSocketError::Io(error) => (format!("I/O ({:?})", error.kind()), true),
            WebSocketError::ConnectionClosed | WebSocketError::AlreadyClosed => {
                ("closed".into(), true)
            }
            WebSocketError::Protocol(_) => ("protocol error".into(), true),
            WebSocketError::Tls(_) => ("TLS error".into(), true),
            WebSocketError::Http(response) => {
                let status = response.status();
                (
                    format!("HTTP {}", status.as_u16()),
                    retryable_status(status),
                )
            }
            _ => ("WebSocket error".into(), false),
        };
        Self {
            reason: format!("{context}: {detail}"),
            reconnect,
        }
    }
}

fn retryable_status(status: reqwest::StatusCode) -> bool {
    status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS
}

impl From<String> for SessionFailure {
    fn from(reason: String) -> Self {
        Self {
            reason,
            reconnect: false,
        }
    }
}

impl From<&str> for SessionFailure {
    fn from(reason: &str) -> Self {
        reason.to_owned().into()
    }
}

async fn report_failure(
    host: &HostHandle,
    state: &watch::Sender<DotsStatePayload>,
    reason: String,
) {
    tracing::warn!(%reason, "Remote MCP connection failed");
    publish(
        host,
        state,
        DotsStatePayload {
            status: DotsConnectionStatus::Failed { reason },
            ..Default::default()
        },
    )
    .await;
}

async fn maintain_connection(
    host: &HostHandle,
    state: &watch::Sender<DotsStatePayload>,
    path: &std::path::Path,
    origin: &url::Url,
    heartbeat_ticks: Option<&tokio::sync::Semaphore>,
) {
    let mut delay = INITIAL_RECONNECT_DELAY;
    publish(
        host,
        state,
        DotsStatePayload {
            status: DotsConnectionStatus::Connecting { last_failure: None },
            ..Default::default()
        },
    )
    .await;
    loop {
        let mut healthy_since = None;
        let Err(failure) = session(
            host,
            state,
            path,
            origin,
            heartbeat_ticks,
            &mut healthy_since,
        )
        .await;
        if !failure.reconnect {
            report_failure(host, state, failure.reason).await;
            return;
        }
        tracing::warn!(reason = %failure.reason, "Reconnecting Remote MCP");
        publish(
            host,
            state,
            DotsStatePayload {
                status: DotsConnectionStatus::Connecting {
                    last_failure: Some(failure.reason),
                },
                ..Default::default()
            },
        )
        .await;
        if healthy_since.is_some_and(|since| since.elapsed() >= MAX_RECONNECT_DELAY) {
            delay = INITIAL_RECONNECT_DELAY;
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(MAX_RECONNECT_DELAY);
    }
}

async fn disconnect(path: &std::path::Path, origin: &url::Url) -> Result<(), String> {
    if !path.exists() {
        return Ok(());
    }
    let credential = credentials(path)?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| "Cannot initialize Dots connection")?;
    let response = client
        .delete(
            origin
                .join(&format!("hosts/{}", credential.host_id))
                .map_err(|_| "Invalid Dots service origin")?,
        )
        .bearer_auth(&credential.host_key)
        .send()
        .await
        .map_err(|_| "Cannot revoke Dots access; retry before pairing again")?;
    if !response.status().is_success() {
        return Err(format!(
            "Dots revocation failed (HTTP {})",
            response.status().as_u16()
        ));
    }
    std::fs::remove_file(path).map_err(|_| "Cannot remove revoked Dots credentials".into())
}

async fn session(
    host: &HostHandle,
    state: &watch::Sender<DotsStatePayload>,
    path: &std::path::Path,
    origin: &url::Url,
    heartbeat_ticks: Option<&tokio::sync::Semaphore>,
    healthy_since: &mut Option<tokio::time::Instant>,
) -> Result<std::convert::Infallible, SessionFailure> {
    let parent = path
        .parent()
        .ok_or("Dots credential directory is missing")?;
    std::fs::create_dir_all(parent).map_err(|_| "Cannot create Dots credential directory")?;
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let lease = options
        .open(path.with_extension("lock"))
        .map_err(|_| "Cannot open Dots connection lock")?;
    fs2::FileExt::try_lock_exclusive(&lease)
        .map_err(|_| "Another Tyde host owns this Dots connection")?;
    let credential = credentials(path)?;
    if [&credential.host_id, &credential.host_key]
        .iter()
        .any(|value| value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()))
    {
        return Err("Invalid Dots host credentials".into());
    }
    let code = random()?;
    let pairing_hash = format!("{:x}", Sha256::digest(code.as_bytes()));
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| "Cannot initialize Dots connection")?;
    let response = client
        .post(
            origin
                .join("hosts")
                .map_err(|_| "Invalid Dots service origin")?,
        )
        .json(&protocol::DotsHostRegistrationPayload {
            host_id: credential.host_id.clone(),
            host_key: credential.host_key.clone(),
            pairing_hash,
        })
        .send()
        .await
        .map_err(|_| SessionFailure::transport("Cannot reach the Remote MCP relay"))?;
    if !response.status().is_success() {
        let status = response.status();
        let reason = format!("Remote MCP registration failed (HTTP {})", status.as_u16());
        return Err(if retryable_status(status) {
            SessionFailure::transport(reason)
        } else {
            reason.into()
        });
    }
    let mut websocket = origin
        .join(&format!("hosts/{}/connect", credential.host_id))
        .map_err(|_| "Invalid Dots connection URL")?;
    websocket
        .set_scheme(if origin.scheme() == "https" {
            "wss"
        } else {
            "ws"
        })
        .map_err(|_| "Invalid Dots connection scheme")?;
    let mut request = websocket
        .as_str()
        .into_client_request()
        .map_err(|_| "Invalid Dots connection request")?;
    request.headers_mut().insert(
        "Authorization",
        format!("Bearer {}", credential.host_key)
            .parse()
            .map_err(|_| "Invalid Dots connection credential")?,
    );
    let (socket, _) = tokio::time::timeout(Duration::from_secs(20), connect_async(request))
        .await
        .map_err(|_| SessionFailure::transport("Remote MCP WebSocket upgrade timed out"))?
        .map_err(|error| SessionFailure::websocket("Remote MCP WebSocket upgrade failed", error))?;
    let (mut sink, mut stream) = socket.split();
    let mut tasks = tokio::task::JoinSet::new();
    let mut paired = false;
    let mut relay_ready = false;
    let mut pong_received = false;
    let mut published_pairing = None;
    let mut ping_counter = 0_u64;
    let mut awaiting_pong = None;
    let mut heartbeat = tokio::time::interval(HEARTBEAT);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let deadline = tokio::time::sleep(Duration::from_secs(600));
    tokio::pin!(deadline);
    let mcp_url = origin
        .join("mcp")
        .map_err(|_| "Invalid Dots MCP URL")?
        .to_string();
    loop {
        tokio::select! {
            _ = &mut deadline, if !paired => return Err("Pairing code expired. Choose Retry to generate a new code.".into()),
            tick = async {
                if let Some(ticks) = heartbeat_ticks.filter(|_| ping_counter != 0) {
                    ticks.acquire().await.map_err(|_| "Remote MCP test heartbeat stopped")?.forget();
                } else {
                    heartbeat.tick().await;
                }
                Ok::<(), &str>(())
            } => {
                tick?;
                if awaiting_pong.is_some() {
                    return Err(SessionFailure::transport("Remote MCP relay missed a WebSocket heartbeat"));
                }
                if pong_received && !relay_ready {
                    return Err(SessionFailure::transport("Remote MCP relay did not acknowledge the connection"));
                }
                ping_counter = ping_counter.wrapping_add(1);
                let nonce = ping_counter.to_be_bytes();
                awaiting_pong = Some(nonce);
                // Keep the read-side heartbeat running even if the peer stops draining writes.
                tokio::time::timeout(HEARTBEAT, sink.send(Message::Ping(nonce.to_vec().into())))
                    .await
                    .map_err(|_| SessionFailure::transport("Remote MCP heartbeat write timed out"))?
                    .map_err(|error| SessionFailure::websocket("Cannot send Remote MCP heartbeat", error))?;
            }
            completed = tasks.join_next(), if !tasks.is_empty() => {
                let reply = completed.ok_or("Dots request task disappeared")?.map_err(|_| "Dots request task failed")??;
                let bytes = serde_json::to_string(&reply).map_err(|_| "Cannot encode Dots response")?;
                tokio::time::timeout(HEARTBEAT, sink.send(Message::Text(bytes.into())))
                    .await
                    .map_err(|_| SessionFailure::transport("Remote MCP response write timed out"))?
                    .map_err(|error| SessionFailure::websocket("Cannot return the Remote MCP response", error))?;
            }
            message = stream.next() => {
                let message = message.ok_or_else(|| SessionFailure::transport("Remote MCP WebSocket ended"))?
                    .map_err(|error| SessionFailure::websocket("Remote MCP WebSocket interrupted", error))?;
                match message {
                    Message::Text(bytes) => {
                        if bytes.len() > 300000 { return Err("Dots service exceeded the request size limit".into()); }
                        let frame: DotsRelayFrame = serde_json::from_str(&bytes).map_err(|_| "Dots service sent an invalid transport frame")?;
                        match frame {
                            DotsRelayFrame::Connection { paired: connected } => {
                                paired = connected;
                                relay_ready = true;
                            }
                            DotsRelayFrame::Request { transport_id, body } => {
                                if !paired { return Err("Dots service attempted a command before pairing".into()); }
                                if tasks.len() >= 32 { return Err("Dots service exceeded the concurrent request limit".into()); }
                                let host = host.clone();
                                tasks.spawn(async move { execute(host, transport_id, body).await });
                            }
                            DotsRelayFrame::Response { .. } => return Err("Dots service sent a host-only transport response".into()),
                        }
                    }
                    Message::Ping(_) => {
                        // Tungstenite queued the protocol pong while reading the ping.
                        tokio::time::timeout(HEARTBEAT, sink.flush())
                            .await
                            .map_err(|_| SessionFailure::transport("Remote MCP pong write timed out"))?
                            .map_err(|error| SessionFailure::websocket("Cannot acknowledge Remote MCP heartbeat", error))?;
                    }
                    Message::Pong(bytes) => {
                        if awaiting_pong.is_some_and(|nonce| bytes.as_ref() == nonce) {
                            awaiting_pong = None;
                            pong_received = true;
                        }
                    }
                    Message::Close(frame) => {
                        let reason = match frame {
                            Some(frame) => format!("Remote MCP relay closed the WebSocket (code {})", u16::from(frame.code)),
                            None => "Remote MCP relay closed the WebSocket".into(),
                        };
                        return Err(SessionFailure::transport(reason));
                    }
                    Message::Binary(_) | Message::Frame(_) => return Err("Dots service sent an unsupported transport frame".into()),
                }
                if relay_ready && pong_received && published_pairing != Some(paired) {
                    published_pairing = Some(paired);
                    healthy_since.get_or_insert_with(tokio::time::Instant::now);
                    publish(host, state, DotsStatePayload {
                        status: if paired { DotsConnectionStatus::Connected } else { DotsConnectionStatus::AwaitingPairing },
                        mcp_url: Some(mcp_url.clone()),
                        pairing_code: if paired { None } else { Some(code.clone()) },
                    }).await;
                }
            }
        }
    }
}

async fn execute(
    host: HostHandle,
    transport_id: String,
    body: String,
) -> Result<DotsRelayFrame, String> {
    use rmcp::{
        RoleServer,
        model::{ClientJsonRpcMessage, ServerJsonRpcMessage},
        transport::OneshotTransport,
    };
    let message: ClientJsonRpcMessage = match serde_json::from_str(&body) {
        Ok(message) => message,
        Err(_) => {
            return Ok(DotsRelayFrame::Response {
                transport_id,
                status: 400,
                body: "{\"error\":\"invalid_mcp_request\"}".into(),
            });
        }
    };
    if !matches!(message, ClientJsonRpcMessage::Request(_)) {
        return Ok(DotsRelayFrame::Response {
            transport_id,
            status: 202,
            body: String::new(),
        });
    }
    let (transport, mut output) = OneshotTransport::<RoleServer>::new(message);
    let service = rmcp::service::serve_directly(DotsMcpServer::new(host), transport, None);
    let result = loop {
        let Some(message) = output.recv().await else {
            break Err("Dots MCP operation ended without a result".to_owned());
        };
        if matches!(
            message,
            ServerJsonRpcMessage::Response(_) | ServerJsonRpcMessage::Error(_)
        ) {
            break serde_json::to_string(&message)
                .map_err(|_| "Cannot encode MCP result".to_owned());
        }
    };
    service
        .cancel()
        .await
        .map_err(|_| "Cannot stop completed Dots MCP operation")?;
    match result {
        Ok(body) if body.len() <= 1000000 => Ok(DotsRelayFrame::Response {
            transport_id,
            status: 200,
            body,
        }),
        Ok(_) => Ok(DotsRelayFrame::Response {
            transport_id,
            status: 413,
            body: "{\"error\":\"result_too_large\"}".into(),
        }),
        Err(reason) => {
            tracing::warn!(%reason, "Remote MCP operation failed");
            Ok(DotsRelayFrame::Response {
                transport_id,
                status: 504,
                body: "{\"error\":\"operation_failed\"}".into(),
            })
        }
    }
}
