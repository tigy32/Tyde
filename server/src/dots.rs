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
    tungstenite::{Message, client::IntoClientRequest},
};
use tokio_util::sync::CancellationToken;

pub(crate) const SERVICE_ORIGIN: &str = "https://mcp.tycode.dev";

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
}

async fn receive_command(
    commands: &mut mpsc::UnboundedReceiver<RuntimeCommand>,
    gate: &mut Option<watch::Receiver<bool>>,
) -> Option<RuntimeCommand> {
    let command = commands.recv().await?;
    if let Some(gate) = gate {
        while !*gate.borrow_and_update() {
            if gate.changed().await.is_err() {
                return None;
            }
        }
    }
    Some(command)
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
    mut complete: watch::Receiver<bool>,
) -> Result<(), String> {
    let ConnectionConfig {
        path,
        origin,
        mut configuration_gate,
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
        let mut connect = enabled;
        loop {
            if connect {
                connect = false;
                publish(
                    &host,
                    &state,
                    DotsStatePayload {
                        status: DotsConnectionStatus::Connecting,
                        ..Default::default()
                    },
                )
                .await;
                let operation = session(&host, &state, &path, &origin);
                tokio::pin!(operation);
                tokio::select! {
                    _ = stopped.cancelled() => break,
                    command = receive_command(&mut commands, &mut configuration_gate) => {
                        match command {
                            Some(RuntimeCommand::Configure(value)) => { enabled = value; connect = enabled; if !enabled { publish(&host, &state, DotsStatePayload::default()).await; } }
                            Some(RuntimeCommand::User(DotsCommandPayload::Retry)) => connect = enabled,
                            Some(RuntimeCommand::User(DotsCommandPayload::Disconnect)) => {
                                if let Err(reason) = disconnect(&path, &origin).await {
                                    publish(&host, &state, DotsStatePayload { status: DotsConnectionStatus::Failed { reason }, ..Default::default() }).await;
                                } else { connect = enabled; }
                            }
                            None => break,
                        }
                    }
                    result = &mut operation => {
                        if let Err(reason) = result {
                            tracing::warn!("Dots host connection failed; user-visible failure published");
                            publish(&host, &state, DotsStatePayload { status: DotsConnectionStatus::Failed { reason }, ..Default::default() }).await;
                        }
                    }
                }
            }
            if connect {
                continue;
            }
            tokio::select! {
                _ = stopped.cancelled() => break,
                command = receive_command(&mut commands, &mut configuration_gate) => match command {
                    Some(RuntimeCommand::Configure(value)) => { enabled = value; connect = enabled; if !enabled { publish(&host, &state, DotsStatePayload::default()).await; } }
                    Some(RuntimeCommand::User(DotsCommandPayload::Retry)) => connect = enabled,
                    Some(RuntimeCommand::User(DotsCommandPayload::Disconnect)) => {
                        match disconnect(&path, &origin).await {
                            Ok(()) => { connect = enabled; if !enabled { publish(&host, &state, DotsStatePayload::default()).await; } }
                            Err(reason) => publish(&host, &state, DotsStatePayload { status: DotsConnectionStatus::Failed { reason }, ..Default::default() }).await,
                        }
                    }
                    None => break,
                }
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
                        while !*complete.borrow_and_update() {
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
) -> Result<(), String> {
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
        .map_err(|_| "Cannot reach the Tyde Dots connection service")?;
    if !response.status().is_success() {
        return Err(format!(
            "Dots setup failed (HTTP {})",
            response.status().as_u16()
        ));
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
        .map_err(|_| "Dots connection timed out")?
        .map_err(|_| "Cannot open the Dots host connection")?;
    let (mut sink, mut stream) = socket.split();
    let mut tasks = tokio::task::JoinSet::new();
    let mut paired = false;
    let deadline = tokio::time::sleep(Duration::from_secs(600));
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = &mut deadline, if !paired => return Err("Pairing code expired. Choose Retry to generate a new code.".into()),
            completed = tasks.join_next(), if !tasks.is_empty() => {
                let reply = completed.ok_or("Dots request task disappeared")?.map_err(|_| "Dots request task failed")??;
                let bytes = serde_json::to_string(&reply).map_err(|_| "Cannot encode Dots response")?;
                sink.send(Message::Text(bytes.into())).await.map_err(|_| "Cannot return the Dots response")?;
            }
            message = stream.next() => {
                let message = message.ok_or("Dots connection closed. Choose Retry to reconnect.")?.map_err(|_| "Dots connection interrupted. Choose Retry to reconnect.")?;
                match message {
                    Message::Text(bytes) => {
                        if bytes.len() > 300000 { return Err("Dots service exceeded the request size limit".into()); }
                        let frame: DotsRelayFrame = serde_json::from_str(&bytes).map_err(|_| "Dots service sent an invalid transport frame")?;
                        match frame {
                            DotsRelayFrame::Connection { paired: connected } => {
                                paired = connected;
                                publish(host, state, DotsStatePayload {
                                    status: if paired { DotsConnectionStatus::Connected } else { DotsConnectionStatus::AwaitingPairing },
                                    mcp_url: Some(origin.join("mcp").map_err(|_| "Invalid Dots MCP URL")?.to_string()),
                                    pairing_code: if paired { None } else { Some(code.clone()) },
                                }).await;
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
                    Message::Ping(bytes) => sink.send(Message::Pong(bytes)).await.map_err(|_| "Cannot acknowledge Dots connection")?,
                    Message::Pong(_) => {},
                    Message::Close(_) => return Err("Dots connection closed. Choose Retry to reconnect.".into()),
                    Message::Binary(_) | Message::Frame(_) => return Err("Dots service sent an unsupported transport frame".into()),
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
    let result = tokio::time::timeout(Duration::from_secs(20), async {
        while let Some(message) = output.recv().await {
            if matches!(
                message,
                ServerJsonRpcMessage::Response(_) | ServerJsonRpcMessage::Error(_)
            ) {
                return serde_json::to_string(&message)
                    .map_err(|_| "Cannot encode MCP result".to_owned());
            }
        }
        Err("Dots MCP operation ended without a result".to_owned())
    })
    .await;
    service
        .cancel()
        .await
        .map_err(|_| "Cannot stop completed Dots MCP operation")?;
    match result {
        Ok(Ok(body)) if body.len() <= 1000000 => Ok(DotsRelayFrame::Response {
            transport_id,
            status: 200,
            body,
        }),
        Ok(Ok(_)) => Ok(DotsRelayFrame::Response {
            transport_id,
            status: 413,
            body: "{\"error\":\"result_too_large\"}".into(),
        }),
        Ok(Err(_)) | Err(_) => Ok(DotsRelayFrame::Response {
            transport_id,
            status: 504,
            body: "{\"error\":\"operation_failed\"}".into(),
        }),
    }
}
