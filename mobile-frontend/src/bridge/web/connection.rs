//! Browser (PWA) connection manager.
//!
//! Ports `mobile/src-tauri/src/mqtt_connection.rs` to the wasm single-context
//! model. Behaviour preserved: connect → run the newline-delimited host-line
//! loop → reconnect with [`MqttReconnectBackoff`] on retryable drops, surfacing
//! the same `host-line` / `host-disconnected` / `host-error` /
//! connection-status events the Tauri shell emits.
//!
//! Simplifications vs. the native shell, justified by the web model: in Tauri
//! the manager runs in a *separate* native process that outlives webview
//! reloads, so it buffers host lines and replays them across frontend
//! reattach (`pending_host_lines`, delivery-id ack, `frontend_attached`). In the
//! browser the manager and the Leptos app share one wasm context that is torn
//! down together on reload — there is no detached frontend to buffer for — so
//! host lines are delivered straight to the live listener with `delivery_id:
//! None`, and the ack / pending-line / replay surface is a no-op.
//!
//! Dropping the ack/replay buffer loses no data because a browser page-reload
//! does not *resume* the old session: it tears down this wasm context entirely
//! and, on the next connect, performs a fresh Tyde handshake whose bootstrap
//! re-syncs the full host state (sessions, projects, agents, …). There is no
//! mid-session continuation to preserve across the reload, so there is nothing
//! the native buffer would have protected. The connection-instance-id handshake
//! the app uses to drop stale lines within a live session is still kept.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use host_config::{HostDisconnectedEvent, HostErrorEvent, HostLineEvent};
use mobile_shell_types::{
    LocalHostId, PairedHostConnectionStatus, PairedHostConnectionStatusEvent,
};
use mqtt_transport::{
    MqttConnectConfig, MqttReconnectBackoff, MqttTransportError, ParticipantRole, PreSharedKey,
};
use protocol::MobileAccessErrorCode;
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

use super::events;
use super::store::{IndexedDbHostStore, IndexedDbPskStore, PskStore, WebPairedHostRecord};

#[cfg(not(target_arch = "wasm32"))]
use tokio::time::{sleep, timeout};
#[cfg(target_arch = "wasm32")]
use wasmtimer::tokio::{sleep, timeout};

const CONNECTION_CHANNEL_CAPACITY: usize = 256;
const CONNECT_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Clone)]
struct StoredConnectionStatus {
    status: PairedHostConnectionStatus,
    connection_instance_id: Option<u64>,
}

struct ActiveConnection {
    tx: mpsc::Sender<ConnectionCommand>,
    actor_instance_id: u64,
    connection_instance_id: Option<u64>,
}

enum ConnectionCommand {
    SendLine {
        line: String,
        reply: oneshot::Sender<Result<(), String>>,
    },
    Stop,
}

#[derive(Default)]
struct ManagerInner {
    active: HashMap<LocalHostId, ActiveConnection>,
    statuses: HashMap<LocalHostId, StoredConnectionStatus>,
    next_connection_instance_id: u64,
    next_actor_instance_id: u64,
}

#[derive(Clone)]
pub struct ConnectionManager {
    inner: Rc<RefCell<ManagerInner>>,
}

thread_local! {
    static MANAGER: ConnectionManager = ConnectionManager {
        inner: Rc::new(RefCell::new(ManagerInner::default())),
    };
}

pub fn manager() -> ConnectionManager {
    MANAGER.with(Clone::clone)
}

impl ConnectionManager {
    pub async fn connect(&self, local_host_id: LocalHostId) -> Result<(), String> {
        if self.inner.borrow().active.contains_key(&local_host_id) {
            return Ok(());
        }
        self.spawn_connection(local_host_id).await
    }

    pub fn disconnect(&self, local_host_id: LocalHostId) -> Result<(), String> {
        let active = self
            .inner
            .borrow_mut()
            .active
            .remove(&local_host_id)
            .ok_or_else(|| format!("paired host {local_host_id} has no active connection"))?;
        self.set_status_and_emit(
            local_host_id.clone(),
            PairedHostConnectionStatus::Disconnected {
                reason: "disconnect requested".to_owned(),
            },
            None,
        );
        let _ = active.tx.try_send(ConnectionCommand::Stop);
        Ok(())
    }

    pub async fn send_line(&self, local_host_id: LocalHostId, line: String) -> Result<(), String> {
        let tx = self
            .inner
            .borrow()
            .active
            .get(&local_host_id)
            .map(|active| active.tx.clone())
            .ok_or_else(|| format!("paired host {local_host_id} has no active connection"))?;
        let (reply, response) = oneshot::channel();
        tx.send(ConnectionCommand::SendLine { line, reply })
            .await
            .map_err(|_| format!("connection actor for {local_host_id} stopped"))?;
        response
            .await
            .map_err(|_| format!("connection actor for {local_host_id} stopped"))?
    }

    pub fn connection_statuses(&self) -> Vec<PairedHostConnectionStatusEvent> {
        self.inner
            .borrow()
            .statuses
            .iter()
            .map(|(local_host_id, stored)| PairedHostConnectionStatusEvent {
                local_host_id: local_host_id.clone(),
                status: stored.status.clone(),
                connection_instance_id: stored.connection_instance_id,
            })
            .collect()
    }

    /// Re-emits the current connection statuses. The browser has no detached
    /// frontend to reconcile, so (unlike Tauri) this never restarts connections.
    pub fn frontend_attached(&self) {
        let statuses = self.connection_statuses();
        for event in statuses {
            events::emit_connection_status(event);
        }
    }

    async fn spawn_connection(&self, local_host_id: LocalHostId) -> Result<(), String> {
        let host_store = IndexedDbHostStore;
        let record = host_store
            .get(&local_host_id)
            .await?
            .ok_or_else(|| format!("paired host {local_host_id} was not found"))?;
        let psk = IndexedDbPskStore.load(&record.psk_keychain_key_id).await?;

        let actor_instance_id = {
            let mut inner = self.inner.borrow_mut();
            let id = inner.next_actor_instance_id;
            inner.next_actor_instance_id = inner.next_actor_instance_id.wrapping_add(1);
            id
        };
        let (tx, rx) = mpsc::channel(CONNECTION_CHANNEL_CAPACITY);
        self.inner.borrow_mut().active.insert(
            local_host_id.clone(),
            ActiveConnection {
                tx,
                actor_instance_id,
                connection_instance_id: None,
            },
        );
        self.set_status_and_emit(
            local_host_id.clone(),
            PairedHostConnectionStatus::Connecting,
            None,
        );

        let manager = self.clone();
        wasm_bindgen_futures::spawn_local(async move {
            run_connection_actor(manager.clone(), record, psk, actor_instance_id, rx).await;
            manager.actor_ended(local_host_id, actor_instance_id);
        });
        Ok(())
    }

    fn allocate_connection_instance_id(&self) -> u64 {
        let mut inner = self.inner.borrow_mut();
        let id = inner.next_connection_instance_id;
        inner.next_connection_instance_id = inner.next_connection_instance_id.wrapping_add(1);
        id
    }

    fn is_current_actor(&self, local_host_id: &LocalHostId, actor_instance_id: u64) -> bool {
        self.inner
            .borrow()
            .active
            .get(local_host_id)
            .is_some_and(|active| active.actor_instance_id == actor_instance_id)
    }

    fn actor_ended(&self, local_host_id: LocalHostId, actor_instance_id: u64) {
        let should_mark = {
            let mut inner = self.inner.borrow_mut();
            if inner
                .active
                .get(&local_host_id)
                .is_some_and(|active| active.actor_instance_id == actor_instance_id)
            {
                let mark = inner.statuses.get(&local_host_id).is_none_or(|stored| {
                    matches!(
                        stored.status,
                        PairedHostConnectionStatus::Connecting
                            | PairedHostConnectionStatus::Connected
                    )
                });
                inner.active.remove(&local_host_id);
                mark
            } else {
                false
            }
        };
        if should_mark {
            self.set_status_and_emit(
                local_host_id,
                PairedHostConnectionStatus::Disconnected {
                    reason: "connection actor ended".to_owned(),
                },
                None,
            );
        }
    }

    fn set_status_and_emit(
        &self,
        local_host_id: LocalHostId,
        status: PairedHostConnectionStatus,
        connection_instance_id: Option<u64>,
    ) {
        self.inner.borrow_mut().statuses.insert(
            local_host_id.clone(),
            StoredConnectionStatus {
                status: status.clone(),
                connection_instance_id,
            },
        );
        if matches!(
            status,
            PairedHostConnectionStatus::Disconnected { .. }
                | PairedHostConnectionStatus::Failed { .. }
        ) {
            events::emit_host_disconnected(HostDisconnectedEvent {
                host_id: local_host_id.0.clone(),
            });
        }
        events::emit_connection_status(PairedHostConnectionStatusEvent {
            local_host_id,
            status,
            connection_instance_id,
        });
    }

    fn emit_connecting(&self, local_host_id: &LocalHostId, actor_instance_id: u64) {
        if !self.is_current_actor(local_host_id, actor_instance_id) {
            return;
        }
        self.set_status_and_emit(
            local_host_id.clone(),
            PairedHostConnectionStatus::Connecting,
            None,
        );
    }

    /// Marks the connection live: allocates a connection-instance id, records it
    /// on the active entry, emits `Connected`, and persists `last_connected_at`.
    /// Returns the new connection-instance id, or `None` if this actor is stale.
    async fn on_connected(
        &self,
        local_host_id: &LocalHostId,
        actor_instance_id: u64,
    ) -> Option<u64> {
        if !self.is_current_actor(local_host_id, actor_instance_id) {
            return None;
        }
        let connection_instance_id = self.allocate_connection_instance_id();
        if let Some(active) = self.inner.borrow_mut().active.get_mut(local_host_id) {
            active.connection_instance_id = Some(connection_instance_id);
        }
        self.set_status_and_emit(
            local_host_id.clone(),
            PairedHostConnectionStatus::Connected,
            Some(connection_instance_id),
        );
        if let Err(error) = IndexedDbHostStore
            .set_last_connected_at_ms(local_host_id, Some(now_ms()))
            .await
        {
            log::warn!("failed to persist last_connected_at_ms for {local_host_id}: {error}");
        } else {
            emit_paired_hosts_changed().await;
        }
        Some(connection_instance_id)
    }

    fn emit_host_line(
        &self,
        local_host_id: &LocalHostId,
        actor_instance_id: u64,
        connection_instance_id: u64,
        line: String,
    ) {
        let current = self
            .inner
            .borrow()
            .active
            .get(local_host_id)
            .is_some_and(|active| {
                active.actor_instance_id == actor_instance_id
                    && active.connection_instance_id == Some(connection_instance_id)
            });
        if !current {
            return;
        }
        events::emit_host_line(HostLineEvent {
            host_id: local_host_id.0.clone(),
            line,
            connection_instance_id: Some(connection_instance_id),
            delivery_id: None,
        });
    }

    fn emit_host_error(
        &self,
        local_host_id: &LocalHostId,
        actor_instance_id: u64,
        message: String,
    ) {
        if !self.is_current_actor(local_host_id, actor_instance_id) {
            return;
        }
        events::emit_host_error(HostErrorEvent {
            host_id: local_host_id.0.clone(),
            message,
        });
    }

    fn emit_disconnected(
        &self,
        local_host_id: &LocalHostId,
        actor_instance_id: u64,
        reason: String,
    ) {
        if !self.is_current_actor(local_host_id, actor_instance_id) {
            return;
        }
        self.set_status_and_emit(
            local_host_id.clone(),
            PairedHostConnectionStatus::Disconnected { reason },
            None,
        );
    }

    fn emit_final_failure(
        &self,
        local_host_id: &LocalHostId,
        actor_instance_id: u64,
        error: &ConnectErr,
    ) {
        if !self.is_current_actor(local_host_id, actor_instance_id) {
            return;
        }
        let message = error.to_string();
        self.emit_host_error(local_host_id, actor_instance_id, message.clone());
        self.set_status_and_emit(
            local_host_id.clone(),
            PairedHostConnectionStatus::Failed {
                code: error.error_code(),
                message,
            },
            None,
        );
    }
}

// ── Connection actor ──────────────────────────────────────────────────────

enum ConnectErr {
    Transport(MqttTransportError),
    Io(std::io::Error),
    Timeout,
    NeedsRepair(String),
}

impl std::fmt::Display for ConnectErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport(error) => write!(f, "{error}"),
            Self::Io(error) => write!(f, "I/O error on MQTT Tyde byte stream: {error}"),
            Self::Timeout => write!(
                f,
                "MQTT connection attempt timed out after {CONNECT_ATTEMPT_TIMEOUT:?}"
            ),
            Self::NeedsRepair(message) => write!(f, "{message}"),
        }
    }
}

impl ConnectErr {
    fn is_retryable(&self) -> bool {
        match self {
            Self::Transport(error) => error.is_retryable(),
            Self::Io(error) => io_error_is_retryable(error),
            Self::Timeout => true,
            Self::NeedsRepair(_) => false,
        }
    }

    fn error_code(&self) -> MobileAccessErrorCode {
        match self {
            Self::Transport(error) => transport_error_code(error),
            Self::Io(error) => transport_error_from_io(error)
                .map(transport_error_code)
                .unwrap_or(MobileAccessErrorCode::TransportFailed),
            Self::Timeout => MobileAccessErrorCode::BrokerConnectionFailed,
            Self::NeedsRepair(_) => MobileAccessErrorCode::InvalidConfig,
        }
    }
}

enum ConnectedOutcome {
    StopRequested,
    Disconnected(ConnectErr),
}

async fn run_connection_actor(
    manager: ConnectionManager,
    record: WebPairedHostRecord,
    psk: PreSharedKey,
    actor_instance_id: u64,
    mut rx: mpsc::Receiver<ConnectionCommand>,
) {
    let local_host_id = record.local_host_id.clone();
    let mut backoff = MqttReconnectBackoff::default();
    loop {
        manager.emit_connecting(&local_host_id, actor_instance_id);

        let connect_result = tokio::select! {
            result = connect_once(&record, &psk) => result,
            command = rx.recv() => {
                if handle_command_while_not_connected(command) {
                    manager.emit_disconnected(&local_host_id, actor_instance_id, "disconnected by user".to_owned());
                    return;
                }
                continue;
            }
        };

        let stream = match connect_result {
            Ok(stream) => stream,
            Err(error) => {
                if !error.is_retryable() {
                    manager.emit_final_failure(&local_host_id, actor_instance_id, &error);
                    return;
                }
                manager.emit_host_error(
                    &local_host_id,
                    actor_instance_id,
                    format!("MQTT connection failed; retrying: {error}"),
                );
                if wait_backoff_or_stop(&mut rx, &mut backoff).await {
                    manager.emit_disconnected(
                        &local_host_id,
                        actor_instance_id,
                        "disconnected by user".to_owned(),
                    );
                    return;
                }
                continue;
            }
        };

        backoff.reset();
        let Some(connection_instance_id) = manager
            .on_connected(&local_host_id, actor_instance_id)
            .await
        else {
            return;
        };

        match run_connected_loop(
            &manager,
            &local_host_id,
            actor_instance_id,
            connection_instance_id,
            stream,
            &mut rx,
        )
        .await
        {
            ConnectedOutcome::StopRequested => {
                manager.emit_disconnected(
                    &local_host_id,
                    actor_instance_id,
                    "disconnected by user".to_owned(),
                );
                return;
            }
            ConnectedOutcome::Disconnected(error) => {
                if !error.is_retryable() {
                    manager.emit_final_failure(&local_host_id, actor_instance_id, &error);
                    return;
                }
                manager.emit_connecting(&local_host_id, actor_instance_id);
                manager.emit_host_error(
                    &local_host_id,
                    actor_instance_id,
                    format!("MQTT connection dropped; reconnecting: {error}"),
                );
                if wait_backoff_or_stop(&mut rx, &mut backoff).await {
                    manager.emit_disconnected(
                        &local_host_id,
                        actor_instance_id,
                        "disconnected by user".to_owned(),
                    );
                    return;
                }
            }
        }
    }
}

async fn connect_once(
    record: &WebPairedHostRecord,
    psk: &PreSharedKey,
) -> Result<mqtt_transport::EnvelopeStream, ConnectErr> {
    if !record.broker.url.as_str().starts_with("wss://") {
        return Err(ConnectErr::NeedsRepair(format!(
            "host \"{}\" was paired for a non-WebSocket broker ({}); the browser client requires a wss:// broker — re-pair from the host's QR",
            record.host_label,
            record.broker.url.as_str()
        )));
    }
    let config = MqttConnectConfig {
        endpoint: record.broker.clone(),
        room: record.room,
        psk: psk.clone(),
        role: ParticipantRole::Client,
    };
    match timeout(
        CONNECT_ATTEMPT_TIMEOUT,
        mqtt_transport::connect_ephemeral(config),
    )
    .await
    {
        Ok(Ok(stream)) => Ok(stream),
        Ok(Err(error)) => Err(ConnectErr::Transport(error)),
        Err(_) => Err(ConnectErr::Timeout),
    }
}

async fn run_connected_loop(
    manager: &ConnectionManager,
    local_host_id: &LocalHostId,
    actor_instance_id: u64,
    connection_instance_id: u64,
    stream: mqtt_transport::EnvelopeStream,
    rx: &mut mpsc::Receiver<ConnectionCommand>,
) -> ConnectedOutcome {
    let (read_half, mut write_half) = tokio::io::split(stream);
    let mut lines = BufReader::new(read_half).lines();

    loop {
        tokio::select! {
            read_result = lines.next_line() => {
                match read_result {
                    Ok(None) => return ConnectedOutcome::Disconnected(ConnectErr::Io(
                        std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "MQTT Tyde byte stream closed")
                    )),
                    Ok(Some(line)) => {
                        if line.is_empty() {
                            continue;
                        }
                        manager.emit_host_line(local_host_id, actor_instance_id, connection_instance_id, line);
                    }
                    Err(error) => return ConnectedOutcome::Disconnected(ConnectErr::Io(error)),
                }
            }
            command = rx.recv() => {
                match command {
                    Some(ConnectionCommand::Stop) | None => return ConnectedOutcome::StopRequested,
                    Some(ConnectionCommand::SendLine { line, reply }) => {
                        let send_result = write_host_line(&mut write_half, &line).await;
                        let failed = send_result.is_err();
                        let _ = reply.send(send_result.map_err(|error| error.to_string()));
                        if failed {
                            return ConnectedOutcome::Disconnected(ConnectErr::Io(
                                std::io::Error::new(std::io::ErrorKind::BrokenPipe, "failed to write to MQTT Tyde byte stream")
                            ));
                        }
                    }
                }
            }
        }
    }
}

async fn write_host_line<W>(writer: &mut W, line: &str) -> Result<(), std::io::Error>
where
    W: AsyncWrite + Unpin,
{
    writer.write_all(line.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await
}

fn handle_command_while_not_connected(command: Option<ConnectionCommand>) -> bool {
    match command {
        Some(ConnectionCommand::Stop) | None => true,
        Some(ConnectionCommand::SendLine { reply, .. }) => {
            let _ = reply.send(Err("host is not connected yet".to_owned()));
            false
        }
    }
}

/// Waits out the reconnect backoff. Returns `true` if the user asked to stop.
async fn wait_backoff_or_stop(
    rx: &mut mpsc::Receiver<ConnectionCommand>,
    backoff: &mut MqttReconnectBackoff,
) -> bool {
    let delay = match backoff.next_delay() {
        Ok(delay) => delay,
        Err(_) => Duration::from_secs(1),
    };
    tokio::select! {
        _ = sleep(delay) => false,
        command = rx.recv() => handle_command_while_not_connected(command),
    }
}

fn io_error_is_retryable(error: &std::io::Error) -> bool {
    if let Some(transport) = transport_error_from_io(error) {
        return transport.is_retryable();
    }
    matches!(
        error.kind(),
        std::io::ErrorKind::UnexpectedEof
            | std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::TimedOut
    )
}

fn transport_error_from_io(error: &std::io::Error) -> Option<&MqttTransportError> {
    error
        .get_ref()
        .and_then(|source| source.downcast_ref::<MqttTransportError>())
}

fn transport_error_code(error: &MqttTransportError) -> MobileAccessErrorCode {
    match error {
        MqttTransportError::Configuration { .. } => MobileAccessErrorCode::InvalidConfig,
        MqttTransportError::BrokerConnect { .. }
        | MqttTransportError::Subscribe { .. }
        | MqttTransportError::SubscribeRejected { .. }
        | MqttTransportError::BrokerDisconnected { .. } => {
            MobileAccessErrorCode::BrokerConnectionFailed
        }
        MqttTransportError::Publish { .. } | MqttTransportError::PublishRejected { .. } => {
            MobileAccessErrorCode::BrokerProtocol
        }
        MqttTransportError::Framing(_) | MqttTransportError::RetainedMessage { .. } => {
            MobileAccessErrorCode::TransportFailed
        }
        MqttTransportError::Crypto(_) => MobileAccessErrorCode::CryptoFailed,
        MqttTransportError::ActorClosed => MobileAccessErrorCode::TransportFailed,
    }
}

async fn emit_paired_hosts_changed() {
    match IndexedDbHostStore.list_summaries().await {
        Ok(hosts) => {
            events::emit_paired_hosts_changed(mobile_shell_types::PairedHostsChangedEvent { hosts })
        }
        Err(error) => log::warn!("failed to list paired hosts for changed event: {error}"),
    }
}

fn now_ms() -> u64 {
    js_sys::Date::now() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn needs_repair_is_terminal_and_invalid_config() {
        let error = ConnectErr::NeedsRepair("re-pair required".to_owned());
        assert!(!error.is_retryable());
        assert_eq!(error.error_code(), MobileAccessErrorCode::InvalidConfig);
    }

    #[test]
    fn connect_timeout_is_retryable_broker_failure() {
        let error = ConnectErr::Timeout;
        assert!(error.is_retryable());
        assert_eq!(
            error.error_code(),
            MobileAccessErrorCode::BrokerConnectionFailed
        );
    }

    #[test]
    fn dropped_broker_is_retryable() {
        let error = ConnectErr::Transport(MqttTransportError::BrokerDisconnected {
            reason: "broker closed the socket".to_owned(),
        });
        assert!(error.is_retryable());
        assert_eq!(
            error.error_code(),
            MobileAccessErrorCode::BrokerConnectionFailed
        );
    }

    #[test]
    fn io_eof_carrying_transport_error_is_classified_by_transport() {
        // EnvelopeStream wraps transport failures in `io::Error::other`; the
        // reconnect logic must recover the inner code, not fall back to a raw
        // io-kind classification.
        let wrapped = std::io::Error::other(MqttTransportError::Crypto(
            mqtt_transport::CryptoError::AeadFailure,
        ));
        let error = ConnectErr::Io(wrapped);
        assert_eq!(error.error_code(), MobileAccessErrorCode::CryptoFailed);
    }
}
