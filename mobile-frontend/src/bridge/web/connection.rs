//! Browser (PWA) connection manager.
//!
//! This is the surviving wasm single-context implementation of the mobile
//! connection contract. Behaviour preserved: connect → run the newline-delimited host-line
//! loop → reconnect with [`MqttReconnectBackoff`] on retryable drops, surfacing
//! the same `host-line` / `host-disconnected` / `host-error` /
//! connection-status events consumed by the mobile frontend.
//!
//! The removed native manager ran in a separate process that outlived webview
//! reloads, so it buffered host lines and replayed them across frontend
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
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::time::Duration;

#[cfg(all(test, target_arch = "wasm32"))]
use std::cell::Cell;

use host_config::{HostDisconnectedEvent, HostErrorEvent, HostLineEvent};
use mobile_shell_types::{
    LocalHostId, PairedHostConnectionStatus, PairedHostConnectionStatusEvent,
};
use mqtt_transport::{
    ManagedMqttConnectConfig, MqttReconnectBackoff, MqttTransportError, ParticipantRole,
    PreSharedKey,
};
use protocol::MobileAccessErrorCode;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, watch};

#[cfg(all(test, target_arch = "wasm32"))]
use tokio::sync::oneshot;

use super::events;
use super::service::ManagedCredentialError;
use super::store::{IndexedDbHostStore, IndexedDbPskStore, PskStore, WebPairedHostRecord};
use crate::bridge::{
    Accepted, ConnectionInvalidation, InvalidationRejected, LocalSubmissionId, SendRejected,
    SubmissionTransportOutcome, SubmissionTransportOutcomeEvent,
};

#[cfg(not(target_arch = "wasm32"))]
use tokio::time::{sleep, timeout};
#[cfg(target_arch = "wasm32")]
use wasmtimer::tokio::{sleep, timeout};

const CONNECTION_CHANNEL_CAPACITY: usize = 256;
const CONNECT_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(20);
const WRITER_LIVENESS_DEADLINE: Duration = Duration::from_secs(45);
/// After this many consecutive retryable failures with the same typed error
/// code the actor keeps retrying with backoff but pins a persistent `Failed`
/// status, so the UI shows an actionable card instead of an ambiguous eternal
/// `Connecting` spinner.
const PERSISTENT_FAILURE_THRESHOLD: u32 = 3;

#[cfg(all(test, target_arch = "wasm32"))]
#[derive(Default)]
enum TestSendBehavior {
    #[default]
    Disabled,
    Capture {
        lines: Vec<String>,
        attempts: usize,
    },
    Reject {
        attempts: usize,
    },
    Defer {
        lines: Vec<String>,
        attempts: usize,
        replies: Vec<(
            oneshot::Sender<Result<Accepted, SendRejected>>,
            LocalSubmissionId,
        )>,
    },
}

#[cfg(all(test, target_arch = "wasm32"))]
enum TestSendAction {
    Immediate(Result<Accepted, SendRejected>),
    Deferred(oneshot::Receiver<Result<Accepted, SendRejected>>),
}

#[cfg(all(test, target_arch = "wasm32"))]
thread_local! {
    static TEST_SEND_BEHAVIOR: RefCell<TestSendBehavior> = RefCell::new(TestSendBehavior::Disabled);
    static TEST_SEND_GENERATION: Cell<u64> = const { Cell::new(0) };
}

#[cfg(all(test, target_arch = "wasm32"))]
pub struct TestSendGuard {
    generation: u64,
}

#[cfg(all(test, target_arch = "wasm32"))]
impl Drop for TestSendGuard {
    fn drop(&mut self) {
        TEST_SEND_GENERATION.with(|generation| {
            if generation.get() != self.generation {
                return;
            }
            TEST_SEND_BEHAVIOR.with(|behavior| {
                *behavior.borrow_mut() = TestSendBehavior::Disabled;
            });
            generation.set(self.generation.wrapping_add(1));
        });
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
fn test_send_guard(behavior: TestSendBehavior) -> TestSendGuard {
    let generation = TEST_SEND_GENERATION.with(|current| {
        let next = current.get().wrapping_add(1);
        current.set(next);
        next
    });
    TEST_SEND_BEHAVIOR.with(|current| {
        *current.borrow_mut() = behavior;
    });
    TestSendGuard { generation }
}

#[cfg(all(test, target_arch = "wasm32"))]
pub fn test_clean_sends() -> TestSendGuard {
    test_send_guard(TestSendBehavior::Disabled)
}

#[cfg(all(test, target_arch = "wasm32"))]
pub fn test_capture_sends() -> TestSendGuard {
    test_send_guard(TestSendBehavior::Capture {
        lines: Vec::new(),
        attempts: 0,
    })
}

#[cfg(all(test, target_arch = "wasm32"))]
pub fn test_reject_sends() -> TestSendGuard {
    test_send_guard(TestSendBehavior::Reject { attempts: 0 })
}

#[cfg(all(test, target_arch = "wasm32"))]
pub fn test_defer_sends() -> TestSendGuard {
    test_send_guard(TestSendBehavior::Defer {
        lines: Vec::new(),
        attempts: 0,
        replies: Vec::new(),
    })
}

#[cfg(all(test, target_arch = "wasm32"))]
pub fn test_resolve_next_send() {
    TEST_SEND_BEHAVIOR.with(|behavior| {
        let TestSendBehavior::Defer { replies, .. } = &mut *behavior.borrow_mut() else {
            panic!("test send behavior is not deferred");
        };
        let (reply, local_submission_id) = replies.remove(0);
        let _ = reply.send(Ok(Accepted {
            connection_instance_id: 1,
            local_submission_id,
        }));
    });
}

#[cfg(all(test, target_arch = "wasm32"))]
pub fn test_sent_lines() -> Vec<String> {
    TEST_SEND_BEHAVIOR.with(|behavior| match &*behavior.borrow() {
        TestSendBehavior::Capture { lines, .. } | TestSendBehavior::Defer { lines, .. } => {
            lines.clone()
        }
        _ => Vec::new(),
    })
}

#[cfg(all(test, target_arch = "wasm32"))]
pub fn test_send_attempts() -> usize {
    TEST_SEND_BEHAVIOR.with(|behavior| match &*behavior.borrow() {
        TestSendBehavior::Capture { attempts, .. }
        | TestSendBehavior::Reject { attempts }
        | TestSendBehavior::Defer { attempts, .. } => *attempts,
        TestSendBehavior::Disabled => 0,
    })
}

#[cfg(all(test, target_arch = "wasm32"))]
fn test_send_action(line: &str) -> Option<TestSendAction> {
    TEST_SEND_BEHAVIOR.with(|behavior| match &mut *behavior.borrow_mut() {
        TestSendBehavior::Disabled => None,
        TestSendBehavior::Capture { lines, attempts } => {
            *attempts += 1;
            lines.push(line.to_owned());
            Some(TestSendAction::Immediate(Ok(Accepted {
                connection_instance_id: 1,
                local_submission_id: LocalSubmissionId(*attempts as u64),
            })))
        }
        TestSendBehavior::Reject { attempts } => {
            *attempts += 1;
            Some(TestSendAction::Immediate(Err(
                SendRejected::ConnectionClosed,
            )))
        }
        TestSendBehavior::Defer {
            lines,
            attempts,
            replies,
        } => {
            *attempts += 1;
            lines.push(line.to_owned());
            let (reply, response) = oneshot::channel();
            replies.push((reply, LocalSubmissionId(*attempts as u64)));
            Some(TestSendAction::Deferred(response))
        }
    })
}

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

type TransportOutcomeCallback = Rc<dyn Fn(SubmissionTransportOutcomeEvent)>;

#[derive(Default)]
struct TransportOutcomeListeners {
    next_id: u64,
    callbacks: Vec<(u64, TransportOutcomeCallback)>,
}

thread_local! {
    static TRANSPORT_OUTCOME_LISTENERS: RefCell<TransportOutcomeListeners> =
        RefCell::new(TransportOutcomeListeners::default());
}

pub fn on_submission_transport_outcome(
    callback: impl Fn(SubmissionTransportOutcomeEvent) + 'static,
) -> impl FnOnce() {
    let callback = Rc::new(callback) as TransportOutcomeCallback;
    let id = TRANSPORT_OUTCOME_LISTENERS.with(|listeners| {
        let mut listeners = listeners.borrow_mut();
        let id = listeners.next_id;
        listeners.next_id = listeners.next_id.wrapping_add(1);
        listeners.callbacks.push((id, callback));
        id
    });
    move || {
        TRANSPORT_OUTCOME_LISTENERS.with(|listeners| {
            listeners
                .borrow_mut()
                .callbacks
                .retain(|(existing, _)| *existing != id);
        });
    }
}

fn emit_submission_transport_outcome(event: SubmissionTransportOutcomeEvent) {
    log::info!(
        "mobile_submission_transport host={} connection_instance_id={} local_submission_id={} outcome={:?}",
        event.local_host_id,
        event.connection_instance_id,
        event.local_submission_id.0,
        event.outcome,
    );
    let callbacks = TRANSPORT_OUTCOME_LISTENERS.with(|listeners| {
        listeners
            .borrow()
            .callbacks
            .iter()
            .map(|(_, callback)| callback.clone())
            .collect::<Vec<_>>()
    });
    for callback in callbacks {
        callback(event.clone());
    }
}

#[derive(Clone)]
struct StoredConnectionStatus {
    status: PairedHostConnectionStatus,
    connection_instance_id: Option<u64>,
}

struct ActiveConnection {
    tx: mpsc::Sender<ConnectionCommand>,
    control: watch::Sender<ConnectionControl>,
    actor_instance_id: u64,
    connection_instance_id: Option<u64>,
}

enum ConnectionCommand {
    SendLine {
        line: String,
        connection_instance_id: u64,
        local_submission_id: LocalSubmissionId,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum ConnectionControl {
    #[default]
    Running,
    Stop,
    Invalidate(ConnectionInvalidation),
}

#[derive(Default)]
struct ManagerInner {
    active: HashMap<LocalHostId, ActiveConnection>,
    statuses: HashMap<LocalHostId, StoredConnectionStatus>,
    next_connection_instance_id: u64,
    next_actor_instance_id: u64,
    next_local_submission_id: u64,
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
        active.control.send_replace(ConnectionControl::Stop);
        log::info!("mobile_connection_control host={local_host_id} control=Stop signalled=true");
        Ok(())
    }

    pub fn invalidate(
        &self,
        local_host_id: &LocalHostId,
        reason: ConnectionInvalidation,
    ) -> Result<(), InvalidationRejected> {
        let control = self
            .inner
            .borrow()
            .active
            .get(local_host_id)
            .and_then(|active| {
                active
                    .connection_instance_id
                    .map(|_| active.control.clone())
            })
            .ok_or(InvalidationRejected::NotConnected)?;
        if control.is_closed() {
            return Err(InvalidationRejected::ConnectionClosed);
        }
        log::error!(
            "mobile_connection_control host={local_host_id} control=Invalidate reason={reason}"
        );
        control.send_replace(ConnectionControl::Invalidate(reason));
        Ok(())
    }

    pub async fn send_line(
        &self,
        local_host_id: LocalHostId,
        line: String,
    ) -> Result<Accepted, SendRejected> {
        #[cfg(all(test, target_arch = "wasm32"))]
        if let Some(action) = test_send_action(&line) {
            return match action {
                TestSendAction::Immediate(result) => result,
                TestSendAction::Deferred(response) => {
                    response.await.map_err(|_| SendRejected::ConnectionClosed)?
                }
            };
        }

        let (tx, connection_instance_id, local_submission_id) = {
            let mut inner = self.inner.borrow_mut();
            let Some(active) = inner.active.get(&local_host_id) else {
                log::warn!(
                    "mobile_send_rejected host={local_host_id} reason={:?} queue_depth=0",
                    SendRejected::NotConnected
                );
                return Err(SendRejected::NotConnected);
            };
            let Some(connection_instance_id) = active.connection_instance_id else {
                log::warn!(
                    "mobile_send_rejected host={local_host_id} reason={:?} queue_depth=0",
                    SendRejected::NotConnected
                );
                return Err(SendRejected::NotConnected);
            };
            let tx = active.tx.clone();
            let local_submission_id = LocalSubmissionId(inner.next_local_submission_id);
            inner.next_local_submission_id = inner.next_local_submission_id.wrapping_add(1);
            (tx, connection_instance_id, local_submission_id)
        };
        match tx.try_send(ConnectionCommand::SendLine {
            line,
            connection_instance_id,
            local_submission_id,
        }) {
            Ok(()) => {
                let queue_depth = CONNECTION_CHANNEL_CAPACITY.saturating_sub(tx.capacity());
                log::info!(
                    "mobile_send_admission host={local_host_id} result=Accepted outcome={:?} connection_instance_id={connection_instance_id} local_submission_id={} queue_depth={queue_depth}",
                    SubmissionTransportOutcome::QueuedLocally,
                    local_submission_id.0,
                );
                emit_submission_transport_outcome(SubmissionTransportOutcomeEvent {
                    local_host_id,
                    connection_instance_id,
                    local_submission_id,
                    outcome: SubmissionTransportOutcome::QueuedLocally,
                });
                Ok(Accepted {
                    connection_instance_id,
                    local_submission_id,
                })
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                log::warn!(
                    "mobile_send_rejected host={local_host_id} reason={:?} queue_depth={CONNECTION_CHANNEL_CAPACITY}",
                    SendRejected::QueueFull
                );
                Err(SendRejected::QueueFull)
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                log::warn!(
                    "mobile_send_rejected host={local_host_id} reason={:?} queue_depth=0",
                    SendRejected::ConnectionClosed
                );
                Err(SendRejected::ConnectionClosed)
            }
        }
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
        let (control, control_rx) = watch::channel(ConnectionControl::Running);
        self.inner.borrow_mut().active.insert(
            local_host_id.clone(),
            ActiveConnection {
                tx,
                control: control.clone(),
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
            run_connection_actor(
                manager.clone(),
                record,
                psk,
                actor_instance_id,
                rx,
                control,
                control_rx,
            )
            .await;
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

    fn clear_connection_instance_id(&self, local_host_id: &LocalHostId, actor_instance_id: u64) {
        if let Some(active) = self.inner.borrow_mut().active.get_mut(local_host_id)
            && active.actor_instance_id == actor_instance_id
        {
            active.connection_instance_id = None;
        }
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
        log::warn!(
            "mobile host {local_host_id} has failed {attempts} consecutive reconnect attempts; \
             still retrying: {error}"
        );
        self.emit_connecting(local_host_id, actor_instance_id);
    }
}

// ── Connection actor ──────────────────────────────────────────────────────

enum ConnectErr {
    Transport(MqttTransportError),
    Io(std::io::Error),
    Timeout,
    WriterDeadline {
        local_submission_id: LocalSubmissionId,
    },
    Invalidated(ConnectionInvalidation),
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
            Self::WriterDeadline {
                local_submission_id,
            } => write!(
                f,
                "MQTT writer work for local submission {} exceeded the session liveness deadline \
                 of {WRITER_LIVENESS_DEADLINE:?}",
                local_submission_id.0
            ),
            Self::Invalidated(reason) => write!(f, "connected session invalidated: {reason}"),
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
            Self::Timeout | Self::WriterDeadline { .. } | Self::Invalidated(_) => true,
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
            Self::Timeout => MobileAccessErrorCode::TransportFailed,
            Self::WriterDeadline { .. } => MobileAccessErrorCode::BrokerProtocol,
            Self::Invalidated(ConnectionInvalidation::HeartbeatTimeout { .. }) => {
                MobileAccessErrorCode::TransportFailed
            }
            Self::Invalidated(_) => MobileAccessErrorCode::BrokerProtocol,
            Self::NeedsRepair(_) => MobileAccessErrorCode::RepairRequired,
            Self::ManagedCredentials(error) => error.code,
        }
    }
}

enum ConnectedOutcome {
    StopRequested,
    Disconnected(ConnectErr),
}

enum ConnectWaitOutcome {
    Connected(Result<mqtt_transport::EnvelopeStream, ConnectErr>),
    Control(ConnectionControl),
}

async fn run_connection_actor(
    manager: ConnectionManager,
    record: WebPairedHostRecord,
    psk: PreSharedKey,
    actor_instance_id: u64,
    mut rx: mpsc::Receiver<ConnectionCommand>,
    control_tx: watch::Sender<ConnectionControl>,
    mut control_rx: watch::Receiver<ConnectionControl>,
) {
    let local_host_id = record.local_host_id.clone();
    let mut backoff = MqttReconnectBackoff::default();
    let mut failures = RepeatedFailures::default();
    loop {
        if !failures.is_persistent() {
            manager.emit_connecting(&local_host_id, actor_instance_id);
        }

        let connect_outcome = tokio::select! {
            biased;
            control = next_control(&mut control_rx) => ConnectWaitOutcome::Control(control),
            result = connect_once(&record, &psk) => ConnectWaitOutcome::Connected(result),
        };

        let stream = match connect_outcome {
            ConnectWaitOutcome::Connected(Ok(stream)) => stream,
            ConnectWaitOutcome::Connected(Err(error)) => {
                if !handle_retryable_connect_failure(
                    &manager,
                    &local_host_id,
                    actor_instance_id,
                    &mut failures,
                    &error,
                ) {
                    return;
                }
                match wait_backoff_or_control(&mut control_rx, &mut backoff).await {
                    ConnectionControl::Running => {}
                    ConnectionControl::Stop => {
                        manager.emit_disconnected(
                            &local_host_id,
                            actor_instance_id,
                            "disconnected by user".to_owned(),
                        );
                        return;
                    }
                    ConnectionControl::Invalidate(reason) => {
                        log::error!(
                            "mobile_connection_control host={local_host_id} control=Invalidate reason={reason} while=reconnect_backoff"
                        );
                        control_tx.send_replace(ConnectionControl::Running);
                    }
                }
                continue;
            }
            ConnectWaitOutcome::Control(ConnectionControl::Stop) => {
                manager.emit_disconnected(
                    &local_host_id,
                    actor_instance_id,
                    "disconnected by user".to_owned(),
                );
                return;
            }
            ConnectWaitOutcome::Control(ConnectionControl::Invalidate(reason)) => {
                control_tx.send_replace(ConnectionControl::Running);
                let error = ConnectErr::Invalidated(reason);
                if !handle_retryable_connect_failure(
                    &manager,
                    &local_host_id,
                    actor_instance_id,
                    &mut failures,
                    &error,
                ) {
                    return;
                }
                match wait_backoff_or_control(&mut control_rx, &mut backoff).await {
                    ConnectionControl::Running => {}
                    ConnectionControl::Stop => {
                        manager.emit_disconnected(
                            &local_host_id,
                            actor_instance_id,
                            "disconnected by user".to_owned(),
                        );
                        return;
                    }
                    ConnectionControl::Invalidate(next_reason) => {
                        log::error!(
                            "mobile_connection_control host={local_host_id} control=Invalidate reason={next_reason} while=reconnect_backoff"
                        );
                        control_tx.send_replace(ConnectionControl::Running);
                    }
                }
                continue;
            }
            ConnectWaitOutcome::Control(ConnectionControl::Running) => continue,
        };

        backoff.reset();
        failures.reset();
        let Some(connection_instance_id) = manager
            .on_connected(&local_host_id, actor_instance_id)
            .await
        else {
            return;
        };

        let outcome = run_connected_loop(
            ConnectedSessionContext {
                manager: &manager,
                local_host_id: &local_host_id,
                actor_instance_id,
                connection_instance_id,
                writer_deadline: WRITER_LIVENESS_DEADLINE,
            },
            stream,
            &mut rx,
            &mut control_rx,
        )
        .await;
        manager.clear_connection_instance_id(&local_host_id, actor_instance_id);

        match outcome {
            ConnectedOutcome::StopRequested => {
                manager.emit_disconnected(
                    &local_host_id,
                    actor_instance_id,
                    "disconnected by user".to_owned(),
                );
                return;
            }
            ConnectedOutcome::Disconnected(error) => {
                if matches!(&error, ConnectErr::Invalidated(_)) {
                    control_tx.send_replace(ConnectionControl::Running);
                }
                if !handle_retryable_connect_failure(
                    &manager,
                    &local_host_id,
                    actor_instance_id,
                    &mut failures,
                    &error,
                ) {
                    return;
                }
                match wait_backoff_or_control(&mut control_rx, &mut backoff).await {
                    ConnectionControl::Running => {}
                    ConnectionControl::Stop => {
                        manager.emit_disconnected(
                            &local_host_id,
                            actor_instance_id,
                            "disconnected by user".to_owned(),
                        );
                        return;
                    }
                    ConnectionControl::Invalidate(reason) => {
                        log::error!(
                            "mobile_connection_control host={local_host_id} control=Invalidate reason={reason} while=reconnect_backoff"
                        );
                        control_tx.send_replace(ConnectionControl::Running);
                    }
                }
            }
        }
    }
}

fn handle_retryable_connect_failure(
    manager: &ConnectionManager,
    local_host_id: &LocalHostId,
    actor_instance_id: u64,
    failures: &mut RepeatedFailures,
    error: &ConnectErr,
) -> bool {
    if !error.is_retryable() {
        manager.emit_final_failure(local_host_id, actor_instance_id, error);
        return false;
    }
    let attempts = failures.record(error.error_code());
    if failures.is_persistent() {
        manager.emit_persistent_failure(local_host_id, actor_instance_id, error, attempts);
    } else {
        log::warn!("MQTT connection to {local_host_id} failed; retrying: {error}");
        manager.emit_connecting(local_host_id, actor_instance_id);
    }
    true
}

async fn connect_once(
    record: &WebPairedHostRecord,
    psk: &PreSharedKey,
) -> Result<mqtt_transport::EnvelopeStream, ConnectErr> {
    match &record.managed {
        Some(_) => connect_managed_once(record, psk).await,
        None => Err(ConnectErr::NeedsRepair(format!(
            "\"{}\" was paired before managed mobile access and can't connect anymore. Re-pair from the host's current QR code in the Mobile tab under Settings (Settings → Mobile) to move it to managed access, or forget it.",
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

enum WriteAttemptFailure {
    Io(std::io::Error),
    Deadline,
}

struct ConnectedSessionContext<'a> {
    manager: &'a ConnectionManager,
    local_host_id: &'a LocalHostId,
    actor_instance_id: u64,
    connection_instance_id: u64,
    writer_deadline: Duration,
}

struct CompletedWrite<S> {
    writer: tokio::io::WriteHalf<S>,
    result: Result<(), WriteAttemptFailure>,
}

type PendingWrite<S> = Pin<Box<dyn Future<Output = CompletedWrite<S>>>>;

async fn run_connected_loop<S>(
    context: ConnectedSessionContext<'_>,
    stream: S,
    rx: &mut mpsc::Receiver<ConnectionCommand>,
    control_rx: &mut watch::Receiver<ConnectionControl>,
) -> ConnectedOutcome
where
    S: AsyncRead + AsyncWrite + Unpin + 'static,
{
    let ConnectedSessionContext {
        manager,
        local_host_id,
        actor_instance_id,
        connection_instance_id,
        writer_deadline,
    } = context;
    let (read_half, write_half) = tokio::io::split(stream);
    let mut lines = BufReader::new(read_half).lines();
    let mut write_half = Some(write_half);
    let mut in_flight = None;
    let mut write_future: Option<PendingWrite<S>> = None;

    loop {
        // A ready Stop/invalidation must win this poll even when inbound,
        // writer, and data work remain continuously ready.
        tokio::select! {
            biased;
            control = next_control(control_rx) => {
                match control {
                    ConnectionControl::Running => continue,
                    ConnectionControl::Stop => {
                        settle_connected_teardown(local_host_id, in_flight.take(), rx);
                        log::info!(
                            "mobile_connection_control host={local_host_id} control=Stop received=true"
                        );
                        return ConnectedOutcome::StopRequested;
                    }
                    ConnectionControl::Invalidate(reason) => {
                        settle_connected_teardown(local_host_id, in_flight.take(), rx);
                        return ConnectedOutcome::Disconnected(ConnectErr::Invalidated(reason));
                    }
                }
            }
            write_result = async {
                match write_future.as_mut() {
                    Some(future) => Some(future.await),
                    None => std::future::pending().await,
                }
            }, if write_future.is_some() => {
                let Some(CompletedWrite { writer, result }) = write_result else {
                    settle_connected_teardown(local_host_id, in_flight.take(), rx);
                    return ConnectedOutcome::Disconnected(ConnectErr::Io(std::io::Error::other(
                        "mobile writer readiness changed before completion",
                    )));
                };
                write_future = None;
                let Some((submission_connection_id, local_submission_id)) = in_flight.take() else {
                    settle_connected_teardown(local_host_id, None, rx);
                    return ConnectedOutcome::Disconnected(ConnectErr::Io(std::io::Error::other(
                        "mobile writer completed without an in-flight submission",
                    )));
                };
                match result {
                    Ok(()) => {
                        write_half = Some(writer);
                        emit_submission_transport_outcome(SubmissionTransportOutcomeEvent {
                            local_host_id: local_host_id.clone(),
                            connection_instance_id: submission_connection_id,
                            local_submission_id,
                            outcome: SubmissionTransportOutcome::BrokerAcknowledged,
                        });
                    }
                    Err(WriteAttemptFailure::Io(error)) => {
                        emit_submission_transport_outcome(SubmissionTransportOutcomeEvent {
                            local_host_id: local_host_id.clone(),
                            connection_instance_id: submission_connection_id,
                            local_submission_id,
                            outcome: SubmissionTransportOutcome::DeliveryUnknown,
                        });
                        settle_connected_teardown(local_host_id, None, rx);
                        return ConnectedOutcome::Disconnected(ConnectErr::Io(error));
                    }
                    Err(WriteAttemptFailure::Deadline) => {
                        emit_submission_transport_outcome(SubmissionTransportOutcomeEvent {
                            local_host_id: local_host_id.clone(),
                            connection_instance_id: submission_connection_id,
                            local_submission_id,
                            outcome: SubmissionTransportOutcome::DeliveryUnknown,
                        });
                        settle_connected_teardown(local_host_id, None, rx);
                        log::error!(
                            "mobile_writer_deadline host={local_host_id} local_submission_id={} code={:?} session_cancelled=true",
                            local_submission_id.0,
                            MobileAccessErrorCode::BrokerProtocol,
                        );
                        return ConnectedOutcome::Disconnected(ConnectErr::WriterDeadline {
                            local_submission_id,
                        });
                    }
                }
            }
            read_result = lines.next_line() => {
                match read_result {
                    Ok(None) => {
                        settle_connected_teardown(local_host_id, in_flight.take(), rx);
                        return ConnectedOutcome::Disconnected(ConnectErr::Io(std::io::Error::new(
                            std::io::ErrorKind::UnexpectedEof,
                            "MQTT Tyde byte stream closed",
                        )));
                    }
                    Ok(Some(line)) => {
                        if !line.is_empty() {
                            manager.emit_host_line(
                                local_host_id,
                                actor_instance_id,
                                connection_instance_id,
                                line,
                            );
                        }
                    }
                    Err(error) => {
                        settle_connected_teardown(local_host_id, in_flight.take(), rx);
                        return ConnectedOutcome::Disconnected(ConnectErr::Io(error));
                    }
                }
            }
            command = rx.recv(), if write_future.is_none() => {
                let Some(ConnectionCommand::SendLine {
                    line,
                    connection_instance_id: submission_connection_id,
                    local_submission_id,
                }) = command else {
                    settle_connected_teardown(local_host_id, None, rx);
                    return ConnectedOutcome::StopRequested;
                };
                if submission_connection_id != connection_instance_id {
                    emit_submission_transport_outcome(SubmissionTransportOutcomeEvent {
                        local_host_id: local_host_id.clone(),
                        connection_instance_id: submission_connection_id,
                        local_submission_id,
                        outcome: SubmissionTransportOutcome::DeliveryUnknown,
                    });
                    continue;
                }
                let Some(mut writer) = write_half.take() else {
                    emit_submission_transport_outcome(SubmissionTransportOutcomeEvent {
                        local_host_id: local_host_id.clone(),
                        connection_instance_id: submission_connection_id,
                        local_submission_id,
                        outcome: SubmissionTransportOutcome::DeliveryUnknown,
                    });
                    settle_connected_teardown(local_host_id, None, rx);
                    return ConnectedOutcome::Disconnected(ConnectErr::Io(std::io::Error::other(
                        "mobile writer was unavailable after dequeue",
                    )));
                };
                in_flight = Some((submission_connection_id, local_submission_id));
                // Do not batch mobile writes. One logical line per write+flush is
                // what makes BrokerAcknowledged attributable to this submission.
                write_future = Some(Box::pin(async move {
                    let result = match timeout(writer_deadline, write_host_line(&mut writer, &line)).await {
                        Ok(result) => result.map_err(WriteAttemptFailure::Io),
                        Err(_) => Err(WriteAttemptFailure::Deadline),
                    };
                    CompletedWrite { writer, result }
                }));
            }
        }
    }
}

fn settle_connected_teardown(
    local_host_id: &LocalHostId,
    in_flight: Option<(u64, LocalSubmissionId)>,
    rx: &mut mpsc::Receiver<ConnectionCommand>,
) {
    let delivery_unknown_count = usize::from(in_flight.is_some());
    if let Some((connection_instance_id, local_submission_id)) = in_flight {
        emit_submission_transport_outcome(SubmissionTransportOutcomeEvent {
            local_host_id: local_host_id.clone(),
            connection_instance_id,
            local_submission_id,
            outcome: SubmissionTransportOutcome::DeliveryUnknown,
        });
    }

    let mut not_sent_count = 0;
    while let Ok(ConnectionCommand::SendLine {
        connection_instance_id,
        local_submission_id,
        ..
    }) = rx.try_recv()
    {
        not_sent_count += 1;
        emit_submission_transport_outcome(SubmissionTransportOutcomeEvent {
            local_host_id: local_host_id.clone(),
            connection_instance_id,
            local_submission_id,
            outcome: SubmissionTransportOutcome::NotSent,
        });
    }
    log::info!(
        "mobile_submission_teardown host={local_host_id} outcome={:?} count={not_sent_count} outcome_after_dequeue={:?} count_after_dequeue={delivery_unknown_count}",
        SubmissionTransportOutcome::NotSent,
        SubmissionTransportOutcome::DeliveryUnknown,
    );
}

async fn write_host_line<W>(writer: &mut W, line: &str) -> Result<(), std::io::Error>
where
    W: AsyncWrite + Unpin,
{
    writer.write_all(line.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await
}

async fn next_control(control_rx: &mut watch::Receiver<ConnectionControl>) -> ConnectionControl {
    loop {
        let control = control_rx.borrow_and_update().clone();
        if control != ConnectionControl::Running {
            return control;
        }
        if control_rx.changed().await.is_err() {
            return ConnectionControl::Stop;
        }
    }
}

async fn wait_backoff_or_control(
    control_rx: &mut watch::Receiver<ConnectionControl>,
    backoff: &mut MqttReconnectBackoff,
) -> ConnectionControl {
    let delay = match backoff.next_delay() {
        Ok(delay) => delay,
        Err(_) => Duration::from_secs(1),
    };
    tokio::select! {
        biased;
        control = next_control(control_rx) => control,
        _ = sleep(delay) => ConnectionControl::Running,
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

    fn active_manager(
        capacity: usize,
        connection_instance_id: Option<u64>,
    ) -> (
        ConnectionManager,
        LocalHostId,
        mpsc::Receiver<ConnectionCommand>,
        watch::Receiver<ConnectionControl>,
    ) {
        let manager = ConnectionManager {
            inner: Rc::new(RefCell::new(ManagerInner::default())),
        };
        let host = LocalHostId("host-boundary".to_owned());
        let (tx, rx) = mpsc::channel(capacity);
        let (control, control_rx) = watch::channel(ConnectionControl::Running);
        manager.inner.borrow_mut().active.insert(
            host.clone(),
            ActiveConnection {
                tx,
                control,
                actor_instance_id: 1,
                connection_instance_id,
            },
        );
        (manager, host, rx, control_rx)
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[tokio::test]
    async fn send_admission_is_typed_and_does_not_wait_for_writer() {
        let (manager, host, mut rx, control_rx) = active_manager(1, Some(41));

        let accepted = manager
            .send_line(host, "first".to_owned())
            .await
            .expect("an open queue must accept immediately");

        assert_eq!(accepted.connection_instance_id, 41);
        let command = rx.try_recv().expect("accepted work must be queued");
        let ConnectionCommand::SendLine {
            local_submission_id,
            ..
        } = command;
        assert_eq!(accepted.local_submission_id, local_submission_id);
        drop(control_rx);
    }

    #[cfg(target_arch = "wasm32")]
    #[wasm_bindgen_test::wasm_bindgen_test]
    async fn wasm_send_admission_settles_before_writer_work() {
        let _send_guard = test_clean_sends();
        let (manager, host, mut rx, _) = active_manager(1, Some(41));

        let accepted = manager
            .send_line(host, "first".to_owned())
            .await
            .expect("an open queue must accept immediately");

        assert_eq!(accepted.connection_instance_id, 41);
        assert!(matches!(
            rx.try_recv(),
            Ok(ConnectionCommand::SendLine {
                local_submission_id,
                ..
            }) if local_submission_id == accepted.local_submission_id
        ));
    }

    #[cfg(target_arch = "wasm32")]
    #[wasm_bindgen_test::wasm_bindgen_test]
    async fn wasm_admission_returns_typed_full_closed_and_disconnected_rejections() {
        let _send_guard = test_clean_sends();
        let (manager, host, rx, mut control_rx) = active_manager(1, Some(7));
        let tx = manager
            .inner
            .borrow()
            .active
            .get(&host)
            .expect("active connection")
            .tx
            .clone();
        tx.try_send(ConnectionCommand::SendLine {
            line: "occupied".to_owned(),
            connection_instance_id: 7,
            local_submission_id: LocalSubmissionId(88),
        })
        .expect("test queue has one slot");

        assert_eq!(
            manager.send_line(host.clone(), "next".to_owned()).await,
            Err(SendRejected::QueueFull)
        );
        manager
            .disconnect(host)
            .expect("priority control must remain available");
        assert_eq!(
            control_rx.borrow_and_update().clone(),
            ConnectionControl::Stop
        );
        drop(rx);

        let (closed_manager, closed_host, closed_rx, _) = active_manager(1, Some(9));
        drop(closed_rx);
        assert_eq!(
            closed_manager
                .send_line(closed_host, "closed".to_owned())
                .await,
            Err(SendRejected::ConnectionClosed)
        );

        let (disconnected_manager, disconnected_host, disconnected_rx, _) = active_manager(1, None);
        assert_eq!(
            disconnected_manager
                .send_line(disconnected_host, "disconnected".to_owned())
                .await,
            Err(SendRejected::NotConnected)
        );
        drop(disconnected_rx);
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[tokio::test]
    async fn send_admission_rejects_full_dead_and_disconnected_queues() {
        let (full_manager, full_host, full_rx, _) = active_manager(1, Some(7));
        let full_tx = full_manager
            .inner
            .borrow()
            .active
            .get(&full_host)
            .expect("active connection")
            .tx
            .clone();
        full_tx
            .try_send(ConnectionCommand::SendLine {
                line: "occupied".to_owned(),
                connection_instance_id: 7,
                local_submission_id: LocalSubmissionId(88),
            })
            .expect("test queue has one slot");
        assert_eq!(
            full_manager.send_line(full_host, "next".to_owned()).await,
            Err(SendRejected::QueueFull)
        );
        drop(full_rx);

        let (closed_manager, closed_host, closed_rx, _) = active_manager(1, Some(9));
        drop(closed_rx);
        assert_eq!(
            closed_manager
                .send_line(closed_host, "next".to_owned())
                .await,
            Err(SendRejected::ConnectionClosed)
        );

        let (disconnected_manager, disconnected_host, disconnected_rx, _) = active_manager(1, None);
        assert_eq!(
            disconnected_manager
                .send_line(disconnected_host, "next".to_owned())
                .await,
            Err(SendRejected::NotConnected)
        );
        drop(disconnected_rx);
    }

    #[test]
    fn stop_control_bypasses_a_full_data_queue() {
        let (manager, host, rx, mut control_rx) = active_manager(1, Some(3));
        let data_tx = manager
            .inner
            .borrow()
            .active
            .get(&host)
            .expect("active connection")
            .tx
            .clone();
        data_tx
            .try_send(ConnectionCommand::SendLine {
                line: "occupied".to_owned(),
                connection_instance_id: 3,
                local_submission_id: LocalSubmissionId(1),
            })
            .expect("test queue has one slot");

        manager
            .disconnect(host)
            .expect("priority control must remain available");

        assert_eq!(
            control_rx.borrow_and_update().clone(),
            ConnectionControl::Stop
        );
        drop(rx);
    }

    #[test]
    fn typed_invalidation_is_not_user_stop_and_keeps_actor_for_reconnect() {
        let (manager, host, rx, mut control_rx) = active_manager(1, Some(5));
        let reason = ConnectionInvalidation::SequenceViolation {
            message: "expected 4, got 6".to_owned(),
        };

        manager
            .invalidate(&host, reason.clone())
            .expect("connected session accepts typed invalidation");

        assert_eq!(
            control_rx.borrow_and_update().clone(),
            ConnectionControl::Invalidate(reason)
        );
        assert!(manager.inner.borrow().active.contains_key(&host));
        drop(rx);
    }

    #[test]
    fn teardown_uses_dequeue_boundary_and_never_replays() {
        let local_host_id = LocalHostId("host-outcomes".to_owned());
        let (tx, mut rx) = mpsc::channel(2);
        tx.try_send(ConnectionCommand::SendLine {
            line: "still queued".to_owned(),
            connection_instance_id: 12,
            local_submission_id: LocalSubmissionId(2),
        })
        .expect("test queue has capacity");
        let observed = Rc::new(RefCell::new(Vec::new()));
        let observed_for_listener = observed.clone();
        let unlisten = on_submission_transport_outcome(move |event| {
            observed_for_listener.borrow_mut().push(event);
        });

        settle_connected_teardown(&local_host_id, Some((12, LocalSubmissionId(1))), &mut rx);

        let observed = observed.borrow();
        assert_eq!(observed.len(), 2);
        assert_eq!(
            observed[0].outcome,
            SubmissionTransportOutcome::DeliveryUnknown
        );
        assert_eq!(observed[0].local_submission_id, LocalSubmissionId(1));
        assert_eq!(observed[1].outcome, SubmissionTransportOutcome::NotSent);
        assert_eq!(observed[1].local_submission_id, LocalSubmissionId(2));
        assert!(matches!(
            rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        drop(observed);
        unlisten();
    }

    struct TestStreamState {
        flush_polled: std::cell::Cell<bool>,
        inbound_reads: std::cell::Cell<usize>,
        written: RefCell<Vec<u8>>,
    }

    struct TestStream {
        state: Rc<TestStreamState>,
        flush_ready: bool,
        inbound_after_flush: Option<&'static [u8]>,
        continuous_inbound: bool,
    }

    impl AsyncRead for TestStream {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buffer: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            let Some(inbound) = self.inbound_after_flush else {
                return std::task::Poll::Pending;
            };
            let inbound_reads = self.state.inbound_reads.get();
            if !self.state.flush_polled.get() || (!self.continuous_inbound && inbound_reads > 0) {
                return std::task::Poll::Pending;
            }
            self.state.inbound_reads.set(inbound_reads + 1);
            buffer.put_slice(inbound);
            std::task::Poll::Ready(Ok(()))
        }
    }

    impl AsyncWrite for TestStream {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buffer: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            self.state.written.borrow_mut().extend_from_slice(buffer);
            std::task::Poll::Ready(Ok(buffer.len()))
        }

        fn poll_flush(
            self: Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            self.state.flush_polled.set(true);
            if self.flush_ready {
                std::task::Poll::Ready(Ok(()))
            } else {
                cx.waker().wake_by_ref();
                std::task::Poll::Pending
            }
        }

        fn poll_shutdown(
            self: Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    async fn assert_continuous_pressure_stop_is_prioritized() {
        let (manager, host, mut rx, mut control_rx) = active_manager(3, Some(77));
        let mut accepted = Vec::new();
        for line in ["dequeued", "queued one", "queued two"] {
            accepted.push(
                manager
                    .send_line(host.clone(), line.to_owned())
                    .await
                    .expect("production admission must accept available capacity"),
            );
        }
        assert_eq!(
            manager
                .send_line(host.clone(), "queue full".to_owned())
                .await,
            Err(SendRejected::QueueFull)
        );

        let state = Rc::new(TestStreamState {
            flush_polled: std::cell::Cell::new(false),
            inbound_reads: std::cell::Cell::new(0),
            written: RefCell::new(Vec::new()),
        });
        let stream = TestStream {
            state: state.clone(),
            flush_ready: false,
            inbound_after_flush: Some(b"inbound while flush is pending\n"),
            continuous_inbound: true,
        };
        let host_for_listener = host.clone();
        let manager_for_listener = manager.clone();
        let unlisten_line = events::on_host_line(move |event| {
            if event.host_id == host_for_listener.0 {
                manager_for_listener
                    .disconnect(host_for_listener.clone())
                    .expect("the real manager Stop path must settle the connected session");
            }
        });
        let outcomes = Rc::new(RefCell::new(Vec::new()));
        let outcomes_for_listener = outcomes.clone();
        let unlisten_outcomes = on_submission_transport_outcome(move |event| {
            outcomes_for_listener
                .borrow_mut()
                .push((event.local_submission_id, event.outcome));
        });

        let outcome = run_connected_loop(
            ConnectedSessionContext {
                manager: &manager,
                local_host_id: &host,
                actor_instance_id: 1,
                connection_instance_id: 77,
                writer_deadline: Duration::from_secs(5),
            },
            stream,
            &mut rx,
            &mut control_rx,
        )
        .await;

        assert!(matches!(outcome, ConnectedOutcome::StopRequested));
        assert!(
            state.flush_polled.get(),
            "the first write reached pending flush"
        );
        assert_eq!(
            state.inbound_reads.get(),
            1,
            "Stop must win the next poll despite continuously ready inbound data"
        );
        assert_eq!(&*state.written.borrow(), b"dequeued\n");
        assert_eq!(
            &*outcomes.borrow(),
            &[
                (
                    accepted[0].local_submission_id,
                    SubmissionTransportOutcome::DeliveryUnknown,
                ),
                (
                    accepted[1].local_submission_id,
                    SubmissionTransportOutcome::NotSent,
                ),
                (
                    accepted[2].local_submission_id,
                    SubmissionTransportOutcome::NotSent,
                ),
            ]
        );
        match rx.try_recv() {
            Err(mpsc::error::TryRecvError::Disconnected) => {}
            Err(mpsc::error::TryRecvError::Empty) => {
                panic!("Stop drained the queue but left its data sender connected")
            }
            Ok(ConnectionCommand::SendLine {
                local_submission_id,
                ..
            }) => panic!(
                "Stop left local submission {} queued instead of settling NotSent",
                local_submission_id.0
            ),
        }
        assert!(!manager.inner.borrow().active.contains_key(&host));
        unlisten_line();
        unlisten_outcomes();
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[tokio::test]
    async fn continuous_inbound_and_data_pressure_cannot_delay_stop() {
        assert_continuous_pressure_stop_is_prioritized().await;
    }

    #[cfg(target_arch = "wasm32")]
    #[wasm_bindgen_test::wasm_bindgen_test]
    async fn wasm_continuous_inbound_and_data_pressure_cannot_delay_stop() {
        let _send_guard = test_clean_sends();
        assert_continuous_pressure_stop_is_prioritized().await;
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[tokio::test]
    async fn completed_flush_acknowledges_exactly_one_logical_line() {
        let (manager, host, mut rx, mut control_rx) = active_manager(1, Some(23));
        let control = manager
            .inner
            .borrow()
            .active
            .get(&host)
            .expect("active connection")
            .control
            .clone();
        let accepted = manager
            .send_line(host.clone(), "one logical line".to_owned())
            .await
            .expect("production admission accepts the logical line");
        let state = Rc::new(TestStreamState {
            flush_polled: std::cell::Cell::new(false),
            inbound_reads: std::cell::Cell::new(0),
            written: RefCell::new(Vec::new()),
        });
        let stream = TestStream {
            state: state.clone(),
            flush_ready: true,
            inbound_after_flush: None,
            continuous_inbound: false,
        };
        let observed = Rc::new(RefCell::new(Vec::new()));
        let observed_for_listener = observed.clone();
        let unlisten = on_submission_transport_outcome(move |event| {
            if event.local_submission_id == accepted.local_submission_id {
                observed_for_listener.borrow_mut().push(event.outcome);
                if event.outcome == SubmissionTransportOutcome::BrokerAcknowledged {
                    control.send_replace(ConnectionControl::Stop);
                }
            }
        });

        let outcome = run_connected_loop(
            ConnectedSessionContext {
                manager: &manager,
                local_host_id: &host,
                actor_instance_id: 1,
                connection_instance_id: 23,
                writer_deadline: Duration::from_secs(5),
            },
            stream,
            &mut rx,
            &mut control_rx,
        )
        .await;

        assert!(matches!(outcome, ConnectedOutcome::StopRequested));
        assert_eq!(&*state.written.borrow(), b"one logical line\n");
        assert_eq!(
            &*observed.borrow(),
            &[SubmissionTransportOutcome::BrokerAcknowledged,]
        );
        unlisten();
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[tokio::test]
    async fn writer_deadline_cancels_session_without_resending_queue() {
        let (manager, host, mut rx, mut control_rx) = active_manager(2, Some(31));
        let mut accepted = Vec::new();
        for line in ["dequeued", "queued"] {
            accepted.push(
                manager
                    .send_line(host.clone(), line.to_owned())
                    .await
                    .expect("production admission accepts available capacity"),
            );
        }
        let state = Rc::new(TestStreamState {
            flush_polled: std::cell::Cell::new(false),
            inbound_reads: std::cell::Cell::new(0),
            written: RefCell::new(Vec::new()),
        });
        let stream = TestStream {
            state: state.clone(),
            flush_ready: false,
            inbound_after_flush: None,
            continuous_inbound: false,
        };
        let observed = Rc::new(RefCell::new(Vec::new()));
        let observed_for_listener = observed.clone();
        let unlisten = on_submission_transport_outcome(move |event| {
            observed_for_listener
                .borrow_mut()
                .push((event.local_submission_id, event.outcome));
        });

        let outcome = run_connected_loop(
            ConnectedSessionContext {
                manager: &manager,
                local_host_id: &host,
                actor_instance_id: 1,
                connection_instance_id: 31,
                writer_deadline: Duration::from_millis(1),
            },
            stream,
            &mut rx,
            &mut control_rx,
        )
        .await;

        match outcome {
            ConnectedOutcome::Disconnected(error) => {
                assert_eq!(error.error_code(), MobileAccessErrorCode::BrokerProtocol);
                assert!(error.is_retryable());
            }
            ConnectedOutcome::StopRequested => panic!("deadline must invalidate the session"),
        }
        assert_eq!(
            &*observed.borrow(),
            &[
                (
                    accepted[0].local_submission_id,
                    SubmissionTransportOutcome::DeliveryUnknown,
                ),
                (
                    accepted[1].local_submission_id,
                    SubmissionTransportOutcome::NotSent,
                ),
            ]
        );
        assert!(state.flush_polled.get(), "the writer stalled at flush");
        assert_eq!(&*state.written.borrow(), b"dequeued\n");
        assert!(matches!(
            rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        unlisten();
    }

    #[test]
    fn same_active_data_room_replays_same_connection_instance_id() {
        #[cfg(target_arch = "wasm32")]
        let _send_guard = test_clean_sends();
        let manager = ConnectionManager {
            inner: Rc::new(RefCell::new(ManagerInner::default())),
        };
        let host = LocalHostId("host".to_owned());
        manager.inner.borrow_mut().statuses.insert(
            host.clone(),
            StoredConnectionStatus {
                status: PairedHostConnectionStatus::Connected,
                connection_instance_id: Some(42),
            },
        );

        let events = manager.connection_statuses();

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].local_host_id, host);
        assert_eq!(events[0].status, PairedHostConnectionStatus::Connected);
        assert_eq!(events[0].connection_instance_id, Some(42));
    }

    #[test]
    fn retry_reconnect_gets_new_connection_instance_id() {
        #[cfg(target_arch = "wasm32")]
        let _send_guard = test_clean_sends();
        let manager = ConnectionManager {
            inner: Rc::new(RefCell::new(ManagerInner {
                next_connection_instance_id: 7,
                ..ManagerInner::default()
            })),
        };

        let first = manager.allocate_connection_instance_id();
        let second = manager.allocate_connection_instance_id();

        assert_eq!(first, 7);
        assert_eq!(second, 8);
        assert_ne!(first, second);
    }

    #[test]
    fn terminal_statuses_have_no_connection_instance_id() {
        #[cfg(target_arch = "wasm32")]
        let _send_guard = test_clean_sends();
        let manager = ConnectionManager {
            inner: Rc::new(RefCell::new(ManagerInner::default())),
        };
        let host = LocalHostId("host".to_owned());
        manager.inner.borrow_mut().statuses.insert(
            host,
            StoredConnectionStatus {
                status: PairedHostConnectionStatus::Failed {
                    code: MobileAccessErrorCode::TransportFailed,
                    message: "terminal".to_owned(),
                },
                connection_instance_id: None,
            },
        );

        let events = manager.connection_statuses();

        assert_eq!(events[0].connection_instance_id, None);
    }

    #[test]
    fn needs_repair_is_terminal_and_repair_required() {
        let error = ConnectErr::NeedsRepair("re-pair required".to_owned());
        assert!(!error.is_retryable());
        assert_eq!(error.error_code(), MobileAccessErrorCode::RepairRequired);
    }

    #[test]
    fn final_repair_failure_emits_the_typed_terminal_status_contract() {
        let (manager, host, rx, control_rx) = active_manager(1, None);
        let message = "Open the Mobile tab in Settings (Settings → Mobile) to re-pair.";
        let error = ConnectErr::NeedsRepair(message.to_owned());

        manager.emit_final_failure(&host, 1, &error);

        let statuses = manager.connection_statuses();
        let event = statuses
            .iter()
            .find(|event| event.local_host_id == host)
            .expect("terminal repair status must be emitted for the current actor");
        assert_eq!(event.connection_instance_id, None);
        assert_eq!(
            &event.status,
            &PairedHostConnectionStatus::Failed {
                code: MobileAccessErrorCode::RepairRequired,
                message: message.to_owned(),
            }
        );
        drop(rx);
        drop(control_rx);
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
        let message = error.to_string();
        assert!(
            message.contains("Settings → Mobile"),
            "stored-host repair must point at the tab that contains pairing: {message}"
        );
        assert!(
            message.contains("Mobile tab under Settings"),
            "spoken wording must not depend on the arrow glyph: {message}"
        );
        assert!(
            !message.contains("Settings → Hosts"),
            "the Hosts tab is a dead-end recovery path: {message}"
        );
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

    #[test]
    fn emit_persistent_failure_keeps_reconnecting_status() {
        let manager = ConnectionManager {
            inner: Rc::new(RefCell::new(ManagerInner::default())),
        };
        let host = LocalHostId("h-persistent".to_owned());
        let (tx, rx) = mpsc::channel(1);
        let (control, control_rx) = watch::channel(ConnectionControl::Running);
        manager.inner.borrow_mut().active.insert(
            host.clone(),
            ActiveConnection {
                tx,
                control,
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
        assert_eq!(event.status, PairedHostConnectionStatus::Connecting);

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
        drop(rx);
        drop(control_rx);
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
