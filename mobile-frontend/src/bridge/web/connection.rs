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
    ManagedMqttConnectConfig, MqttReconnectBackoff, MqttTransportError, ParticipantRole,
    PreSharedKey,
};
use protocol::MobileAccessErrorCode;
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

use super::events;
use super::service::ManagedCredentialError;
use super::store::{IndexedDbHostStore, IndexedDbPskStore, PskStore, WebPairedHostRecord};

#[cfg(not(target_arch = "wasm32"))]
use tokio::time::{sleep, timeout};
#[cfg(target_arch = "wasm32")]
use wasmtimer::tokio::{sleep, timeout};

const CONNECTION_CHANNEL_CAPACITY: usize = 256;
const CONNECT_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(20);
/// After this many consecutive retryable failures with the same typed error
/// code the actor keeps retrying with backoff but pins a persistent `Failed`
/// status, so the UI shows an actionable card instead of an ambiguous eternal
/// `Connecting` spinner.
const PERSISTENT_FAILURE_THRESHOLD: u32 = 3;

/// Tracks consecutive retryable failures sharing the same typed
/// [`MobileAccessErrorCode`]. Keyed on the code — NOT the rendered message —
/// because message details vary between attempts (broker disconnect reasons,
/// service error text) and must not keep resetting the count; the latest
/// error's message is still what the persistent card displays. A different
/// code restarts the count (a new situation gets the transient treatment
/// again); a successful connect resets it.
#[derive(Default)]
struct RepeatedFailures {
    code: Option<MobileAccessErrorCode>,
    count: u32,
}

impl RepeatedFailures {
    fn record(&mut self, code: MobileAccessErrorCode) -> u32 {
        if self.code == Some(code) {
            self.count += 1;
        } else {
            self.code = Some(code);
            self.count = 1;
        }
        self.count
    }

    fn reset(&mut self) {
        self.code = None;
        self.count = 0;
    }

    fn is_persistent(&self) -> bool {
        self.count >= PERSISTENT_FAILURE_THRESHOLD
    }
}

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

    /// Repeated same-code retryable failures: the reconnect loop keeps running,
    /// but the surfaced status becomes a persistent `Failed` card (instead of
    /// `Connecting`) that names the latest failure and the attempt count.
    fn emit_persistent_failure(
        &self,
        local_host_id: &LocalHostId,
        actor_instance_id: u64,
        error: &ConnectErr,
        attempts: u32,
    ) {
        if !self.is_current_actor(local_host_id, actor_instance_id) {
            return;
        }
        let message = format!(
            "{error} ({attempts} consecutive attempts failed; still retrying in background)"
        );
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
    /// The managed pairing could not obtain broker credentials from `tycode.dev`
    /// (session expired, service unavailable, pairing revoked, …).
    ManagedCredentials(ManagedCredentialError),
}

impl std::fmt::Display for ConnectErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport(error) => write!(f, "{error}"),
            Self::Io(error) => write!(f, "I/O error on MQTT Tyde byte stream: {error}"),
            Self::Timeout => write!(
                f,
                "MQTT connection attempt timed out after {CONNECT_ATTEMPT_TIMEOUT:?}: no broker \
                 failure was reported, but the host never completed the rendezvous — it may be \
                 offline, asleep, or not running Tyde"
            ),
            Self::NeedsRepair(message) => write!(f, "{message}"),
            Self::ManagedCredentials(error) => write!(f, "{}", error.message),
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
            Self::ManagedCredentials(error) => error.retryable,
        }
    }

    fn error_code(&self) -> MobileAccessErrorCode {
        match self {
            Self::Transport(error) => transport_error_code(error),
            Self::Io(error) => transport_error_from_io(error)
                .map(transport_error_code)
                .unwrap_or(MobileAccessErrorCode::TransportFailed),
            // Not `BrokerConnectionFailed`: broker-level failures are reported
            // as typed `Transport` errors. Hitting this timeout means no broker
            // failure was reported within the window and the rendezvous with
            // the host never completed.
            Self::Timeout => MobileAccessErrorCode::TransportFailed,
            Self::NeedsRepair(_) => MobileAccessErrorCode::RepairRequired,
            Self::ManagedCredentials(error) => error.code,
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
    let mut failures = RepeatedFailures::default();
    loop {
        // Once the failure has repeated into a persistent `Failed` card, the
        // retry loop keeps running silently behind it: re-emitting `Connecting`
        // here would resurrect the ambiguous spinner the card replaced.
        if !failures.is_persistent() {
            manager.emit_connecting(&local_host_id, actor_instance_id);
        }

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
                let attempts = failures.record(error.error_code());
                if failures.is_persistent() {
                    manager.emit_persistent_failure(
                        &local_host_id,
                        actor_instance_id,
                        &error,
                        attempts,
                    );
                } else {
                    manager.emit_host_error(
                        &local_host_id,
                        actor_instance_id,
                        format!("MQTT connection failed; retrying: {error}"),
                    );
                }
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
        failures.reset();
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
                let attempts = failures.record(error.error_code());
                if failures.is_persistent() {
                    manager.emit_persistent_failure(
                        &local_host_id,
                        actor_instance_id,
                        &error,
                        attempts,
                    );
                } else {
                    manager.emit_connecting(&local_host_id, actor_instance_id);
                    manager.emit_host_error(
                        &local_host_id,
                        actor_instance_id,
                        format!("MQTT connection dropped; reconnecting: {error}"),
                    );
                }
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
    // Managed (`tyde-pair://v2`) pairings are the ONLY connectable records. They
    // mint fresh tycode.dev-signed broker credentials and connect over the AWS
    // IoT managed broker with the scanned rendezvous room + PSK.
    match &record.managed {
        Some(_) => connect_managed_once(record, psk).await,
        // Any unmanaged stored record — a retired public broker OR a custom
        // WSS broker from a legacy pairing — fails closed (findings #2/#8,
        // locked decision #8). There is no unmanaged/public-broker connect
        // path; the record must be re-paired through tycode.dev.
        None => Err(ConnectErr::NeedsRepair(format!(
            "\"{}\" was paired before managed mobile access and can't connect anymore. Re-pair from the host's current QR code (Settings → Hosts) to move it to managed access, or forget it.",
            record.host_label
        ))),
    }
}

async fn connect_managed_once(
    record: &WebPairedHostRecord,
    psk: &PreSharedKey,
) -> Result<mqtt_transport::EnvelopeStream, ConnectErr> {
    let (broker, credentials) =
        super::service::obtain_managed_credentials(record, now_ms()).await?;
    let config = ManagedMqttConnectConfig {
        broker,
        credentials,
        room: record.room,
        psk: psk.clone(),
        role: ParticipantRole::Client,
    };
    match timeout(
        CONNECT_ATTEMPT_TIMEOUT,
        mqtt_transport::connect_managed_ephemeral(config),
    )
    .await
    {
        Ok(Ok(stream)) => Ok(stream),
        Ok(Err(error)) => Err(ConnectErr::Transport(error)),
        Err(_) => Err(ConnectErr::Timeout),
    }
}

impl From<ManagedCredentialError> for ConnectErr {
    fn from(error: ManagedCredentialError) -> Self {
        ConnectErr::ManagedCredentials(error)
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
        MqttTransportError::Framing(_)
        | MqttTransportError::RetainedMessage { .. }
        | MqttTransportError::PublishAckMismatch { .. }
        | MqttTransportError::ReceiverCreditTimeout { .. } => {
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
    fn needs_repair_is_terminal_and_repair_required() {
        let error = ConnectErr::NeedsRepair("re-pair required".to_owned());
        assert!(!error.is_retryable());
        assert_eq!(error.error_code(), MobileAccessErrorCode::RepairRequired);
    }

    /// Finding #2: an unmanaged stored record — even a custom `wss://` broker —
    /// must fail closed to a terminal repair-required error, never connect.
    // Native-only: uses the tokio test runtime, which the wasm target lacks.
    #[cfg(not(target_arch = "wasm32"))]
    #[tokio::test]
    async fn unmanaged_custom_wss_record_fails_closed_with_repair() {
        use mqtt_transport::{BrokerAuth, BrokerEndpoint, PreSharedKey, RoomId};
        let record = WebPairedHostRecord {
            local_host_id: LocalHostId("h-legacy".to_owned()),
            host_label: "Legacy Custom Broker".to_owned(),
            broker: BrokerEndpoint {
                url: protocol::BrokerUrl::new("wss://custom.example.test/mqtt").unwrap(),
                auth: BrokerAuth::Anonymous,
            },
            room: RoomId([1_u8; 16]),
            psk_keychain_key_id: mobile_shell_types::KeychainSecretId("k".to_owned()),
            credential_fingerprint: "fp".to_owned(),
            auto_connect: false,
            last_connected_at_ms: None,
            managed: None,
        };
        let psk = PreSharedKey::from_slice(&[2_u8; 32]).unwrap();
        let error = match connect_once(&record, &psk).await {
            Ok(_) => panic!("unmanaged records must fail closed"),
            Err(error) => error,
        };
        assert!(!error.is_retryable(), "repair is terminal, not retryable");
        assert_eq!(error.error_code(), MobileAccessErrorCode::RepairRequired);
    }

    /// The attempt timeout is the "broker never failed, host never answered"
    /// outcome: it must stay retryable but must NOT be coded as a broker
    /// connection failure (typed `Transport` errors own that), and its message
    /// must surface the rendezvous-wait distinction to the user.
    #[test]
    fn connect_timeout_is_retryable_rendezvous_wait_not_broker_failure() {
        let error = ConnectErr::Timeout;
        assert!(error.is_retryable());
        assert_eq!(error.error_code(), MobileAccessErrorCode::TransportFailed);
        let message = error.to_string();
        assert!(
            message.contains("no broker failure was reported"),
            "{message}"
        );
        assert!(
            message.contains("host never completed the rendezvous"),
            "{message}"
        );
    }

    /// Repeated retryable failures with the same typed code cross the
    /// persistence threshold (pinning the actionable `Failed` card); a
    /// different code or a successful connect restarts the transient treatment.
    #[test]
    fn repeated_same_code_failures_become_persistent_and_reset_on_change() {
        let mut failures = RepeatedFailures::default();
        assert_eq!(failures.record(MobileAccessErrorCode::TransportFailed), 1);
        assert!(!failures.is_persistent());
        assert_eq!(failures.record(MobileAccessErrorCode::TransportFailed), 2);
        assert!(!failures.is_persistent());
        assert_eq!(failures.record(MobileAccessErrorCode::TransportFailed), 3);
        assert!(failures.is_persistent());

        assert_eq!(
            failures.record(MobileAccessErrorCode::BrokerConnectionFailed),
            1,
            "a different failure code is a new situation"
        );
        assert!(!failures.is_persistent());
        failures.record(MobileAccessErrorCode::BrokerConnectionFailed);
        failures.record(MobileAccessErrorCode::BrokerConnectionFailed);
        assert!(failures.is_persistent());

        failures.reset();
        assert!(!failures.is_persistent());
        assert_eq!(failures.record(MobileAccessErrorCode::TransportFailed), 1);
    }

    /// Broker failures render attempt-specific detail (disconnect reasons,
    /// service messages); repetition is keyed on the typed code so that
    /// varying detail cannot keep resetting the count and trap the UI in the
    /// eternal `Connecting` spinner the persistent card exists to replace.
    #[test]
    fn variable_error_detail_does_not_reset_persistence_counting() {
        let attempts = [
            "broker closed the socket (epoch 1)",
            "connection reset by peer",
            "keep-alive timeout at 17s",
        ];
        let mut failures = RepeatedFailures::default();
        let mut rendered = Vec::new();
        for (index, reason) in attempts.iter().enumerate() {
            let error = ConnectErr::Transport(MqttTransportError::BrokerDisconnected {
                reason: (*reason).to_owned(),
            });
            rendered.push(error.to_string());
            assert_eq!(failures.record(error.error_code()), index as u32 + 1);
        }
        assert_eq!(
            rendered
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len(),
            attempts.len(),
            "the rendered messages must actually differ for this test to prove anything"
        );
        assert!(
            failures.is_persistent(),
            "same-code failures with different detail must still accumulate"
        );
    }

    /// Emission path: with the actor registered as current, a persistent
    /// failure must pin a `Failed` status (with the typed code and the latest
    /// error's message plus the retrying notice) that `connection_statuses`
    /// then reports — this is what replaces the spinner in the UI.
    #[test]
    fn emit_persistent_failure_pins_failed_status_with_latest_message() {
        let manager = ConnectionManager {
            inner: Rc::new(RefCell::new(ManagerInner::default())),
        };
        let host = LocalHostId("h-persistent".to_owned());
        let (tx, _rx) = mpsc::channel(1);
        manager.inner.borrow_mut().active.insert(
            host.clone(),
            ActiveConnection {
                tx,
                actor_instance_id: 7,
                connection_instance_id: None,
            },
        );

        let error = ConnectErr::Timeout;
        manager.emit_persistent_failure(&host, 7, &error, PERSISTENT_FAILURE_THRESHOLD);

        let statuses = manager.connection_statuses();
        let event = statuses
            .iter()
            .find(|event| event.local_host_id == host)
            .expect("a status must be stored for the failing host");
        match &event.status {
            PairedHostConnectionStatus::Failed { code, message } => {
                assert_eq!(*code, MobileAccessErrorCode::TransportFailed);
                assert!(
                    message.contains("host never completed the rendezvous"),
                    "the latest error's own message must be displayed: {message}"
                );
                assert!(
                    message.contains("still retrying"),
                    "the card must say retries continue: {message}"
                );
                assert!(
                    message.contains(&PERSISTENT_FAILURE_THRESHOLD.to_string()),
                    "the attempt count must be visible: {message}"
                );
            }
            other => panic!("expected a persistent Failed status, got {other:?}"),
        }

        // A stale actor must not be able to pin anything.
        let other_host = LocalHostId("h-stale".to_owned());
        manager.emit_persistent_failure(&other_host, 99, &error, PERSISTENT_FAILURE_THRESHOLD);
        assert!(
            !manager
                .connection_statuses()
                .iter()
                .any(|event| event.local_host_id == other_host),
            "a non-current actor must not store a status"
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

    #[test]
    fn io_eof_carrying_publish_ack_mismatch_is_transport_failed() {
        let wrapped = std::io::Error::other(MqttTransportError::PublishAckMismatch {
            packet_id: Some(9),
            token: None,
        });
        let error = ConnectErr::Io(wrapped);
        assert_eq!(error.error_code(), MobileAccessErrorCode::TransportFailed);
    }
}
