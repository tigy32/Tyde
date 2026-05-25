use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use host_config::{HostDisconnectedEvent, HostErrorEvent, HostLineEvent};
use mqtt_transport::{
    MqttConnectConfig, MqttReconnectBackoff, MqttTransportError, ParticipantRole, PreSharedKey,
    ReconnectBackoffError,
};
use protocol::MobileAccessErrorCode;
use tauri::Emitter;
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

use crate::paired_hosts::{PairedHostRecord, Store, StoreError};
use crate::psk_store::{PskStore, PskStoreError};
use crate::types::{
    LocalHostId, PairedHostConnectionStatus, PairedHostConnectionStatusEvent,
    PairedHostsChangedEvent,
};

const MANAGER_CHANNEL_CAPACITY: usize = 128;
const CONNECTION_CHANNEL_CAPACITY: usize = 256;
const CONNECT_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(20);
pub const HOST_LINE_EVENT: &str = "tyde://host-line";
pub const HOST_DISCONNECTED_EVENT: &str = "tyde://host-disconnected";
pub const HOST_ERROR_EVENT: &str = "tyde://host-error";
pub const PAIRED_HOSTS_CHANGED_EVENT: &str = "tyde://paired-hosts-changed";
pub const PAIRED_HOST_CONNECTION_STATUS_EVENT: &str = "tyde://paired-host-connection-status";

#[derive(Debug, Error)]
pub enum ManagerError {
    #[error("paired host {0} does not have an active connection")]
    ConnectionNotFound(LocalHostId),
    #[error("connection manager actor stopped")]
    ActorStopped,
    #[error("connection manager response channel closed")]
    ResponseClosed,
    #[error("connection actor for paired host {host_id} stopped")]
    ConnectionActorStopped { host_id: LocalHostId },
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    PskStore(#[from] PskStoreError),
    #[error("send_host_line failed for paired host {host_id}: {message}")]
    SendLineFailed {
        host_id: LocalHostId,
        message: String,
    },
}

pub struct Manager {
    tx: mpsc::Sender<ManagerCommand>,
}

impl Manager {
    pub fn start(app: tauri::AppHandle, store: Arc<Store>, psk_store: Arc<dyn PskStore>) -> Self {
        let (tx, rx) = mpsc::channel(MANAGER_CHANNEL_CAPACITY);
        let actor = ManagerActor {
            app,
            store,
            psk_store,
            rx,
            tx: tx.clone(),
            active: HashMap::new(),
            statuses: HashMap::new(),
            pending_host_lines: HashMap::new(),
            next_host_line_delivery_id: 0,
            next_connection_instance_id: 0,
        };
        tauri::async_runtime::spawn(actor.run());
        Self { tx }
    }

    pub async fn connect(&self, local_host_id: LocalHostId) -> Result<(), ManagerError> {
        self.request(|reply| ManagerCommand::Connect {
            local_host_id,
            reply,
        })
        .await
    }

    pub async fn disconnect(&self, local_host_id: LocalHostId) -> Result<(), ManagerError> {
        self.request(|reply| ManagerCommand::Disconnect {
            local_host_id,
            reply,
        })
        .await
    }

    pub async fn send_line(
        &self,
        local_host_id: LocalHostId,
        line: String,
    ) -> Result<(), ManagerError> {
        self.request(|reply| ManagerCommand::SendLine {
            local_host_id,
            line,
            reply,
        })
        .await
    }

    pub async fn connection_statuses(
        &self,
    ) -> Result<Vec<PairedHostConnectionStatusEvent>, ManagerError> {
        self.request(|reply| ManagerCommand::ListConnectionStatuses { reply })
            .await
    }

    pub async fn pending_host_lines(&self) -> Result<Vec<HostLineEvent>, ManagerError> {
        self.request(|reply| ManagerCommand::ListPendingHostLines { reply })
            .await
    }

    pub async fn ack_host_line(
        &self,
        local_host_id: LocalHostId,
        delivery_id: u64,
    ) -> Result<(), ManagerError> {
        self.request(|reply| ManagerCommand::AckHostLine {
            local_host_id,
            delivery_id,
            reply,
        })
        .await
    }

    pub async fn frontend_attached(&self) -> Result<(), ManagerError> {
        self.request(|reply| ManagerCommand::FrontendAttached { reply })
            .await
    }

    async fn request<T>(
        &self,
        make: impl FnOnce(oneshot::Sender<Result<T, ManagerError>>) -> ManagerCommand,
    ) -> Result<T, ManagerError> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(make(reply))
            .await
            .map_err(|_| ManagerError::ActorStopped)?;
        response.await.map_err(|_| ManagerError::ResponseClosed)?
    }
}

enum ManagerCommand {
    Connect {
        local_host_id: LocalHostId,
        reply: oneshot::Sender<Result<(), ManagerError>>,
    },
    Disconnect {
        local_host_id: LocalHostId,
        reply: oneshot::Sender<Result<(), ManagerError>>,
    },
    SendLine {
        local_host_id: LocalHostId,
        line: String,
        reply: oneshot::Sender<Result<(), ManagerError>>,
    },
    ListConnectionStatuses {
        reply: oneshot::Sender<Result<Vec<PairedHostConnectionStatusEvent>, ManagerError>>,
    },
    ConnectionStatusChanged {
        local_host_id: LocalHostId,
        instance_id: u64,
        status: PairedHostConnectionStatus,
    },
    HostLineReceived {
        local_host_id: LocalHostId,
        instance_id: u64,
        line: String,
    },
    HostError {
        local_host_id: LocalHostId,
        instance_id: u64,
        message: String,
    },
    ListPendingHostLines {
        reply: oneshot::Sender<Result<Vec<HostLineEvent>, ManagerError>>,
    },
    AckHostLine {
        local_host_id: LocalHostId,
        delivery_id: u64,
        reply: oneshot::Sender<Result<(), ManagerError>>,
    },
    ActorEnded {
        local_host_id: LocalHostId,
        instance_id: u64,
    },
    FrontendAttached {
        reply: oneshot::Sender<Result<(), ManagerError>>,
    },
}

struct ActiveConnection {
    tx: mpsc::Sender<ConnectionCommand>,
    instance_id: u64,
    _task: tauri::async_runtime::JoinHandle<()>,
}

struct ManagerActor {
    app: tauri::AppHandle,
    store: Arc<Store>,
    psk_store: Arc<dyn PskStore>,
    rx: mpsc::Receiver<ManagerCommand>,
    tx: mpsc::Sender<ManagerCommand>,
    active: HashMap<LocalHostId, ActiveConnection>,
    statuses: HashMap<LocalHostId, PairedHostConnectionStatus>,
    pending_host_lines: HashMap<LocalHostId, VecDeque<BufferedHostLine>>,
    next_host_line_delivery_id: u64,
    next_connection_instance_id: u64,
}

#[derive(Clone)]
struct BufferedHostLine {
    delivery_id: u64,
    line: String,
}

impl ManagerActor {
    async fn run(mut self) {
        while let Some(command) = self.rx.recv().await {
            match command {
                ManagerCommand::Connect {
                    local_host_id,
                    reply,
                } => {
                    let result = self.connect(local_host_id).await;
                    let _send_result = reply.send(result);
                }
                ManagerCommand::Disconnect {
                    local_host_id,
                    reply,
                } => {
                    let result = self.disconnect(local_host_id).await;
                    let _send_result = reply.send(result);
                }
                ManagerCommand::SendLine {
                    local_host_id,
                    line,
                    reply,
                } => {
                    let result = self.send_line(local_host_id, line).await;
                    let _send_result = reply.send(result);
                }
                ManagerCommand::ListConnectionStatuses { reply } => {
                    let result = match self.store.list_records().await {
                        Ok(records) => {
                            let known_ids = records
                                .into_iter()
                                .map(|record| record.local_host_id)
                                .collect::<HashSet<_>>();
                            self.statuses
                                .retain(|local_host_id, _| known_ids.contains(local_host_id));
                            Ok(self
                                .statuses
                                .iter()
                                .map(|(local_host_id, status)| PairedHostConnectionStatusEvent {
                                    local_host_id: local_host_id.clone(),
                                    status: status.clone(),
                                })
                                .collect())
                        }
                        Err(error) => Err(ManagerError::Store(error)),
                    };
                    let _send_result = reply.send(result);
                }
                ManagerCommand::ConnectionStatusChanged {
                    local_host_id,
                    instance_id,
                    status,
                } => {
                    self.connection_status_changed(local_host_id, instance_id, status);
                }
                ManagerCommand::HostLineReceived {
                    local_host_id,
                    instance_id,
                    line,
                } => {
                    self.host_line_received(local_host_id, instance_id, line);
                }
                ManagerCommand::HostError {
                    local_host_id,
                    instance_id,
                    message,
                } => {
                    self.host_error(local_host_id, instance_id, message);
                }
                ManagerCommand::ListPendingHostLines { reply } => {
                    let _send_result = reply.send(Ok(self.pending_host_lines()));
                }
                ManagerCommand::AckHostLine {
                    local_host_id,
                    delivery_id,
                    reply,
                } => {
                    self.ack_host_line(&local_host_id, delivery_id);
                    let _send_result = reply.send(Ok(()));
                }
                ManagerCommand::ActorEnded {
                    local_host_id,
                    instance_id,
                } => {
                    if self
                        .active
                        .get(&local_host_id)
                        .is_some_and(|active| active.instance_id == instance_id)
                    {
                        let should_mark_disconnected =
                            self.statuses.get(&local_host_id).is_none_or(|status| {
                                matches!(
                                    status,
                                    PairedHostConnectionStatus::Connecting
                                        | PairedHostConnectionStatus::Connected
                                )
                            });
                        self.active.remove(&local_host_id);
                        if should_mark_disconnected {
                            self.set_status_and_emit(
                                local_host_id,
                                PairedHostConnectionStatus::Disconnected {
                                    reason: "connection actor ended".to_owned(),
                                },
                            );
                        }
                    }
                }
                ManagerCommand::FrontendAttached { reply } => {
                    let result = self.frontend_attached().await;
                    let _send_result = reply.send(result);
                }
            }
        }
    }

    async fn connect(&mut self, local_host_id: LocalHostId) -> Result<(), ManagerError> {
        if self.active.contains_key(&local_host_id) {
            tracing::info!(
                local_host_id = %local_host_id,
                "connect requested for an already-active paired host"
            );
            return Ok(());
        }
        self.spawn_connection(local_host_id).await
    }

    async fn disconnect(&mut self, local_host_id: LocalHostId) -> Result<(), ManagerError> {
        let active = self
            .active
            .remove(&local_host_id)
            .ok_or_else(|| ManagerError::ConnectionNotFound(local_host_id.clone()))?;
        self.set_status_and_emit(
            local_host_id.clone(),
            PairedHostConnectionStatus::Disconnected {
                reason: "disconnect requested".to_owned(),
            },
        );
        active.tx.send(ConnectionCommand::Stop).await.map_err(|_| {
            ManagerError::ConnectionActorStopped {
                host_id: local_host_id,
            }
        })
    }

    async fn frontend_attached(&mut self) -> Result<(), ManagerError> {
        let host_ids = self.active.keys().cloned().collect::<Vec<_>>();
        tracing::info!(
            active_connections = host_ids.len(),
            "frontend attached; reconciling paired host connections"
        );
        let mut restarted = HashSet::new();
        for local_host_id in host_ids {
            let Some(active) = self.active.remove(&local_host_id) else {
                continue;
            };
            let _send_result = active.tx.send(ConnectionCommand::Stop).await;
            restarted.insert(local_host_id.clone());
            self.spawn_connection(local_host_id).await?;
        }
        let records = self.store.list_records().await?;
        for record in records
            .into_iter()
            .filter(|record| record.auto_connect && !restarted.contains(&record.local_host_id))
        {
            if self.active.contains_key(&record.local_host_id) {
                continue;
            }
            tracing::info!(
                local_host_id = %record.local_host_id,
                "frontend attach found auto-connect host without an active connection; connecting"
            );
            self.spawn_connection(record.local_host_id).await?;
        }
        Ok(())
    }

    async fn spawn_connection(&mut self, local_host_id: LocalHostId) -> Result<(), ManagerError> {
        self.pending_host_lines.remove(&local_host_id);
        let record = self.store.get(local_host_id.clone()).await?;
        let psk = self.psk_store.load(&record.psk_keychain_key_id)?;
        let (tx, rx) = mpsc::channel(CONNECTION_CHANNEL_CAPACITY);
        let app = self.app.clone();
        let store = self.store.clone();
        let manager_tx = self.tx.clone();
        let task_host_id = local_host_id.clone();
        let instance_id = self.allocate_connection_instance_id();
        self.set_status_and_emit(
            local_host_id.clone(),
            PairedHostConnectionStatus::Connecting,
        );
        let task = tauri::async_runtime::spawn(async move {
            run_connection_actor(app, store, record, psk, manager_tx.clone(), instance_id, rx)
                .await;
            let _send_result = manager_tx
                .send(ManagerCommand::ActorEnded {
                    local_host_id: task_host_id,
                    instance_id,
                })
                .await;
        });
        self.active.insert(
            local_host_id,
            ActiveConnection {
                tx,
                instance_id,
                _task: task,
            },
        );
        Ok(())
    }

    fn allocate_connection_instance_id(&mut self) -> u64 {
        let instance_id = self.next_connection_instance_id;
        self.next_connection_instance_id = self
            .next_connection_instance_id
            .checked_add(1)
            .unwrap_or_else(|| {
                tracing::warn!("connection instance id overflow; wrapping to zero");
                0
            });
        instance_id
    }

    async fn send_line(
        &mut self,
        local_host_id: LocalHostId,
        line: String,
    ) -> Result<(), ManagerError> {
        let active = self
            .active
            .get(&local_host_id)
            .ok_or_else(|| ManagerError::ConnectionNotFound(local_host_id.clone()))?;
        let (reply, response) = oneshot::channel();
        active
            .tx
            .send(ConnectionCommand::SendLine { line, reply })
            .await
            .map_err(|_| ManagerError::ConnectionActorStopped {
                host_id: local_host_id.clone(),
            })?;
        response
            .await
            .map_err(|_| ManagerError::ConnectionActorStopped {
                host_id: local_host_id.clone(),
            })?
            .map_err(|message| ManagerError::SendLineFailed {
                host_id: local_host_id,
                message,
            })
    }

    fn connection_status_changed(
        &mut self,
        local_host_id: LocalHostId,
        instance_id: u64,
        status: PairedHostConnectionStatus,
    ) {
        if self
            .active
            .get(&local_host_id)
            .is_none_or(|active| active.instance_id != instance_id)
        {
            tracing::info!(
                local_host_id = %local_host_id,
                instance_id,
                "ignoring stale paired host connection status"
            );
            return;
        }
        self.set_status_and_emit(local_host_id, status);
    }

    fn set_status_and_emit(
        &mut self,
        local_host_id: LocalHostId,
        status: PairedHostConnectionStatus,
    ) {
        if matches!(status, PairedHostConnectionStatus::Connecting) {
            self.pending_host_lines.remove(&local_host_id);
        }
        self.statuses.insert(local_host_id.clone(), status.clone());
        if matches!(
            status,
            PairedHostConnectionStatus::Disconnected { .. }
                | PairedHostConnectionStatus::Failed { .. }
        ) && let Err(error) = self.app.emit(
            HOST_DISCONNECTED_EVENT,
            HostDisconnectedEvent {
                host_id: local_host_id.0.clone(),
            },
        ) {
            tracing::warn!(error = %error, "failed to emit host disconnected event");
        }
        if let Err(error) = self.app.emit(
            PAIRED_HOST_CONNECTION_STATUS_EVENT,
            PairedHostConnectionStatusEvent {
                local_host_id,
                status,
            },
        ) {
            tracing::warn!(error = %error, "failed to emit paired host connection status");
        }
    }

    fn host_error(&mut self, local_host_id: LocalHostId, instance_id: u64, message: String) {
        if self
            .active
            .get(&local_host_id)
            .is_none_or(|active| active.instance_id != instance_id)
        {
            tracing::info!(
                local_host_id = %local_host_id,
                instance_id,
                "ignoring stale host error"
            );
            return;
        }
        if let Err(error) = self.app.emit(
            HOST_ERROR_EVENT,
            HostErrorEvent {
                host_id: local_host_id.0,
                message,
            },
        ) {
            tracing::warn!(error = %error, "failed to emit host error event");
        }
    }

    fn host_line_received(&mut self, local_host_id: LocalHostId, instance_id: u64, line: String) {
        if self
            .active
            .get(&local_host_id)
            .is_none_or(|active| active.instance_id != instance_id)
        {
            tracing::info!(
                local_host_id = %local_host_id,
                instance_id,
                "ignoring stale host line"
            );
            return;
        }
        let delivery_id = self.next_host_line_delivery_id;
        self.next_host_line_delivery_id = self
            .next_host_line_delivery_id
            .checked_add(1)
            .unwrap_or_else(|| {
                tracing::warn!("host-line delivery id overflow; wrapping to zero");
                0
            });
        self.pending_host_lines
            .entry(local_host_id.clone())
            .or_default()
            .push_back(BufferedHostLine {
                delivery_id,
                line: line.clone(),
            });

        if let Err(error) = self.app.emit(
            HOST_LINE_EVENT,
            HostLineEvent {
                host_id: local_host_id.0,
                line,
                delivery_id: Some(delivery_id),
            },
        ) {
            tracing::warn!(error = %error, "failed to emit host line event; line remains pending");
        }
    }

    fn pending_host_lines(&self) -> Vec<HostLineEvent> {
        let mut events = self
            .pending_host_lines
            .iter()
            .flat_map(|(local_host_id, lines)| {
                lines.iter().map(|line| HostLineEvent {
                    host_id: local_host_id.0.clone(),
                    line: line.line.clone(),
                    delivery_id: Some(line.delivery_id),
                })
            })
            .collect::<Vec<_>>();
        events.sort_by_key(|event| event.delivery_id.unwrap_or(u64::MAX));
        events
    }

    fn ack_host_line(&mut self, local_host_id: &LocalHostId, delivery_id: u64) {
        let Some(lines) = self.pending_host_lines.get_mut(local_host_id) else {
            return;
        };
        lines.retain(|line| line.delivery_id != delivery_id);
        if lines.is_empty() {
            self.pending_host_lines.remove(local_host_id);
        }
    }
}

enum ConnectionCommand {
    SendLine {
        line: String,
        reply: oneshot::Sender<Result<(), String>>,
    },
    Stop,
}

#[derive(Debug, Error)]
enum ConnectionError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Transport(#[from] MqttTransportError),
    #[error("I/O error on MQTT Tyde byte stream: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    ReconnectBackoff(#[from] ReconnectBackoffError),
    #[error("MQTT connection attempt timed out after {timeout:?}")]
    ConnectTimeout { timeout: Duration },
    #[error("failed to emit Tauri event {event}: {message}")]
    Emit {
        event: &'static str,
        message: String,
    },
    #[error("system clock is before Unix epoch: {0}")]
    SystemTime(String),
    #[error("current time in milliseconds does not fit in u64")]
    TimeOverflow,
}

enum ConnectedOutcome {
    StopRequested,
    Disconnected(ConnectionError),
}

async fn run_connection_actor(
    app: tauri::AppHandle,
    store: Arc<Store>,
    record: PairedHostRecord,
    psk: PreSharedKey,
    manager_tx: mpsc::Sender<ManagerCommand>,
    instance_id: u64,
    mut rx: mpsc::Receiver<ConnectionCommand>,
) {
    let mut backoff = MqttReconnectBackoff::default();
    loop {
        match store.get(record.local_host_id.clone()).await {
            Ok(_) => {}
            Err(StoreError::HostNotFound(_)) => {
                emit_disconnected(
                    &manager_tx,
                    &record.local_host_id,
                    instance_id,
                    "paired host was removed".to_owned(),
                );
                return;
            }
            Err(error) => {
                emit_final_failure(
                    &manager_tx,
                    &record.local_host_id,
                    instance_id,
                    &ConnectionError::Store(error),
                );
                return;
            }
        }

        emit_connection_status(
            &manager_tx,
            &record.local_host_id,
            instance_id,
            PairedHostConnectionStatus::Connecting,
        );

        let connect_result = tokio::select! {
            result = connect_once(&record, &psk) => result,
            command = rx.recv() => {
                if handle_command_while_not_connected(command).await {
                    emit_disconnected(
                        &manager_tx,
                        &record.local_host_id,
                        instance_id,
                        "disconnected by user".to_owned(),
                    );
                    return;
                }
                continue;
            }
        };

        let stream = match connect_result {
            Ok(stream) => stream,
            Err(error) => {
                if !error_is_retryable(&error) {
                    emit_final_failure(&manager_tx, &record.local_host_id, instance_id, &error);
                    return;
                }
                emit_host_error(
                    &manager_tx,
                    &record.local_host_id,
                    instance_id,
                    format!("MQTT connection failed; retrying: {error}"),
                );
                match wait_backoff_or_stop(&mut rx, &mut backoff).await {
                    Ok(true) => {
                        emit_disconnected(
                            &manager_tx,
                            &record.local_host_id,
                            instance_id,
                            "disconnected by user".to_owned(),
                        );
                        return;
                    }
                    Ok(false) => {}
                    Err(error) => {
                        emit_final_failure(&manager_tx, &record.local_host_id, instance_id, &error);
                        return;
                    }
                }
                continue;
            }
        };

        backoff.reset();
        emit_connection_status(
            &manager_tx,
            &record.local_host_id,
            instance_id,
            PairedHostConnectionStatus::Connected,
        );
        match now_ms() {
            Ok(ms) => match store
                .set_last_connected_at_ms(record.local_host_id.clone(), Some(ms))
                .await
            {
                Ok(_) => emit_paired_hosts_changed(&app, &store).await,
                Err(error) => emit_host_error(
                    &manager_tx,
                    &record.local_host_id,
                    instance_id,
                    format!("failed to persist last_connected_at_ms: {error}"),
                ),
            },
            Err(error) => emit_host_error(
                &manager_tx,
                &record.local_host_id,
                instance_id,
                format!("failed to compute last_connected_at_ms: {error}"),
            ),
        }

        match run_connected_loop(
            &manager_tx,
            &record.local_host_id,
            instance_id,
            stream,
            &mut rx,
        )
        .await
        {
            ConnectedOutcome::StopRequested => {
                emit_disconnected(
                    &manager_tx,
                    &record.local_host_id,
                    instance_id,
                    "disconnected by user".to_owned(),
                );
                return;
            }
            ConnectedOutcome::Disconnected(error) => {
                if !error_is_retryable(&error) {
                    emit_final_failure(&manager_tx, &record.local_host_id, instance_id, &error);
                    return;
                }
                emit_host_error(
                    &manager_tx,
                    &record.local_host_id,
                    instance_id,
                    format!("MQTT connection dropped; reconnecting: {error}"),
                );
                match wait_backoff_or_stop(&mut rx, &mut backoff).await {
                    Ok(true) => {
                        emit_disconnected(
                            &manager_tx,
                            &record.local_host_id,
                            instance_id,
                            "disconnected by user".to_owned(),
                        );
                        return;
                    }
                    Ok(false) => {}
                    Err(error) => {
                        emit_final_failure(&manager_tx, &record.local_host_id, instance_id, &error);
                        return;
                    }
                }
            }
        }
    }
}

async fn connect_once(
    record: &PairedHostRecord,
    psk: &PreSharedKey,
) -> Result<mqtt_transport::EnvelopeStream, ConnectionError> {
    let config = MqttConnectConfig {
        endpoint: record.broker.clone(),
        room: record.room,
        psk: psk.clone(),
        role: ParticipantRole::Client,
    };
    connect_attempt_with_timeout(CONNECT_ATTEMPT_TIMEOUT, async move {
        mqtt_transport::connect_ephemeral(config)
            .await
            .map_err(ConnectionError::Transport)
    })
    .await
}

async fn connect_attempt_with_timeout<T, F>(
    timeout_duration: Duration,
    connect: F,
) -> Result<T, ConnectionError>
where
    F: Future<Output = Result<T, ConnectionError>>,
{
    tokio::time::timeout(timeout_duration, connect)
        .await
        .map_err(|_| ConnectionError::ConnectTimeout {
            timeout: timeout_duration,
        })?
}

async fn run_connected_loop(
    manager_tx: &mpsc::Sender<ManagerCommand>,
    local_host_id: &LocalHostId,
    instance_id: u64,
    stream: mqtt_transport::EnvelopeStream,
    rx: &mut mpsc::Receiver<ConnectionCommand>,
) -> ConnectedOutcome {
    let (read_half, mut write_half) = tokio::io::split(stream);
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();

    loop {
        line.clear();
        tokio::select! {
            read_result = reader.read_line(&mut line) => {
                match read_result {
                    Ok(0) => return ConnectedOutcome::Disconnected(ConnectionError::Io(
                        std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "MQTT Tyde byte stream closed")
                    )),
                    Ok(_) => {
                        trim_line_endings(&mut line);
                        if line.is_empty() {
                            continue;
                        }
                        if let Err(error) =
                            buffer_host_line(manager_tx, local_host_id, instance_id, line.clone()).await
                        {
                            return ConnectedOutcome::Disconnected(error);
                        }
                    }
                    Err(error) => return ConnectedOutcome::Disconnected(ConnectionError::Io(error)),
                }
            }
            command = rx.recv() => {
                match command {
                    Some(ConnectionCommand::Stop) | None => return ConnectedOutcome::StopRequested,
                    Some(ConnectionCommand::SendLine { line, reply }) => {
                        let send_result = write_host_line(&mut write_half, &line).await;
                        let failed = send_result.is_err();
                        let reply_result = send_result.map_err(|error| error.to_string());
                        let _send_result = reply.send(reply_result);
                        if failed {
                            return ConnectedOutcome::Disconnected(ConnectionError::Io(
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

async fn handle_command_while_not_connected(command: Option<ConnectionCommand>) -> bool {
    match command {
        Some(ConnectionCommand::Stop) | None => true,
        Some(ConnectionCommand::SendLine { reply, .. }) => {
            let _send_result = reply.send(Err("host is not connected yet".to_owned()));
            false
        }
    }
}

async fn wait_backoff_or_stop(
    rx: &mut mpsc::Receiver<ConnectionCommand>,
    backoff: &mut MqttReconnectBackoff,
) -> Result<bool, ConnectionError> {
    let delay = backoff.next_delay()?;
    tokio::select! {
        _ = tokio::time::sleep(delay) => Ok(false),
        command = rx.recv() => Ok(handle_command_while_not_connected(command).await),
    }
}

fn trim_line_endings(line: &mut String) {
    if line.ends_with('\n') {
        line.pop();
    }
    if line.ends_with('\r') {
        line.pop();
    }
}

fn error_is_retryable(error: &ConnectionError) -> bool {
    match error {
        ConnectionError::Transport(error) => error.is_retryable(),
        ConnectionError::Io(error) => io_error_is_retryable(error),
        ConnectionError::ConnectTimeout { .. } => true,
        ConnectionError::Store(_)
        | ConnectionError::ReconnectBackoff(_)
        | ConnectionError::Emit { .. }
        | ConnectionError::SystemTime(_)
        | ConnectionError::TimeOverflow => false,
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

fn error_code(error: &ConnectionError) -> MobileAccessErrorCode {
    match error {
        ConnectionError::Store(_) => MobileAccessErrorCode::StoreLoadFailed,
        ConnectionError::Transport(MqttTransportError::Configuration { .. }) => {
            MobileAccessErrorCode::InvalidConfig
        }
        ConnectionError::Transport(transport) => mobile_error_code_for_transport(transport),
        ConnectionError::Io(error) => transport_error_from_io(error)
            .map(mobile_error_code_for_transport)
            .unwrap_or(MobileAccessErrorCode::TransportFailed),
        ConnectionError::ConnectTimeout { .. } => MobileAccessErrorCode::BrokerConnectionFailed,
        ConnectionError::ReconnectBackoff(_)
        | ConnectionError::Emit { .. }
        | ConnectionError::SystemTime(_)
        | ConnectionError::TimeOverflow => MobileAccessErrorCode::Internal,
    }
}

fn mobile_error_code_for_transport(error: &MqttTransportError) -> MobileAccessErrorCode {
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

fn emit_connection_status(
    manager_tx: &mpsc::Sender<ManagerCommand>,
    local_host_id: &LocalHostId,
    instance_id: u64,
    status: PairedHostConnectionStatus,
) {
    if let Err(error) = manager_tx.try_send(ManagerCommand::ConnectionStatusChanged {
        local_host_id: local_host_id.clone(),
        instance_id,
        status,
    }) {
        tracing::warn!(error = %error, "failed to record paired host connection status");
    }
}

async fn buffer_host_line(
    manager_tx: &mpsc::Sender<ManagerCommand>,
    local_host_id: &LocalHostId,
    instance_id: u64,
    line: String,
) -> Result<(), ConnectionError> {
    manager_tx
        .send(ManagerCommand::HostLineReceived {
            local_host_id: local_host_id.clone(),
            instance_id,
            line,
        })
        .await
        .map_err(|error| ConnectionError::Emit {
            event: HOST_LINE_EVENT,
            message: error.to_string(),
        })
}

fn emit_disconnected(
    manager_tx: &mpsc::Sender<ManagerCommand>,
    local_host_id: &LocalHostId,
    instance_id: u64,
    reason: String,
) {
    emit_connection_status(
        manager_tx,
        local_host_id,
        instance_id,
        PairedHostConnectionStatus::Disconnected { reason },
    );
}

fn emit_final_failure(
    manager_tx: &mpsc::Sender<ManagerCommand>,
    local_host_id: &LocalHostId,
    instance_id: u64,
    error: &ConnectionError,
) {
    let message = error.to_string();
    emit_host_error(manager_tx, local_host_id, instance_id, message.clone());
    emit_connection_status(
        manager_tx,
        local_host_id,
        instance_id,
        PairedHostConnectionStatus::Failed {
            code: error_code(error),
            message,
        },
    );
}

fn emit_host_error(
    manager_tx: &mpsc::Sender<ManagerCommand>,
    local_host_id: &LocalHostId,
    instance_id: u64,
    message: String,
) {
    if let Err(error) = manager_tx.try_send(ManagerCommand::HostError {
        local_host_id: local_host_id.clone(),
        instance_id,
        message,
    }) {
        tracing::warn!(error = %error, "failed to record host error");
    }
}

pub async fn emit_paired_hosts_changed(app: &tauri::AppHandle, store: &Store) {
    match store.list_summaries().await {
        Ok(hosts) => {
            if let Err(error) = app.emit(
                PAIRED_HOSTS_CHANGED_EVENT,
                PairedHostsChangedEvent { hosts },
            ) {
                tracing::warn!(error = %error, "failed to emit paired hosts changed event");
            }
        }
        Err(error) => {
            tracing::warn!(error = %error, "failed to list paired hosts for changed event")
        }
    }
}

fn now_ms() -> Result<u64, ConnectionError> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| ConnectionError::SystemTime(error.to_string()))?;
    u64::try_from(duration.as_millis()).map_err(|_| ConnectionError::TimeOverflow)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn trim_line_endings_removes_lf_and_crlf() {
        let mut line = "hello\r\n".to_owned();
        trim_line_endings(&mut line);
        assert_eq!(line, "hello");

        let mut line = "hello\n".to_owned();
        trim_line_endings(&mut line);
        assert_eq!(line, "hello");
    }

    #[test]
    fn reconnect_backoff_caps_at_max() {
        let mut backoff =
            MqttReconnectBackoff::new(Duration::from_secs(16), mqtt_transport::RECONNECT_MAX)
                .expect("valid backoff");
        assert_eq!(backoff.current_base_delay(), Duration::from_secs(16));
        let _ = backoff.next_delay().expect("jitter");
        assert_eq!(backoff.current_base_delay(), mqtt_transport::RECONNECT_MAX);
        let _ = backoff.next_delay().expect("jitter");
        assert_eq!(backoff.current_base_delay(), mqtt_transport::RECONNECT_MAX);
    }

    #[tokio::test]
    async fn connect_attempt_timeout_is_retryable_broker_failure() {
        let error = connect_attempt_with_timeout(
            Duration::from_millis(1),
            std::future::pending::<Result<(), ConnectionError>>(),
        )
        .await
        .expect_err("pending connect attempt should time out");

        assert!(matches!(error, ConnectionError::ConnectTimeout { .. }));
        assert!(error_is_retryable(&error));
        assert_eq!(
            error_code(&error),
            MobileAccessErrorCode::BrokerConnectionFailed
        );
    }
}
