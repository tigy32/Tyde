//! Browser (PWA) connection manager.
//!
//! This is the surviving wasm single-context implementation of the mobile
//! connection contract. Behaviour preserved: connect → run the framed host-record
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

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
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
use tokio::io::{AsyncRead, AsyncWrite, BufReader};
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
    audio_pending: Rc<Cell<u8>>,
}

struct AudioPermit(Rc<Cell<u8>>);
impl Drop for AudioPermit {
    fn drop(&mut self) {
        self.0.set(self.0.get().saturating_sub(1));
    }
}

enum ConnectionCommand {
    SendLine {
        line: String,
        connection_instance_id: u64,
        local_submission_id: LocalSubmissionId,
    },
    SendFrame {
        frame: protocol::ProtocolFrame,
        connection_instance_id: u64,
        local_submission_id: LocalSubmissionId,
        permit: Option<AudioPermit>,
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

    pub async fn send_frame(
        &self,
        local_host_id: LocalHostId,
        frame: protocol::ProtocolFrame,
    ) -> Result<(), SendRejected> {
        let (tx, connection_instance_id, local_submission_id, permit) = {
            let mut inner = self.inner.borrow_mut();
            let active = inner
                .active
                .get(&local_host_id)
                .ok_or(SendRejected::NotConnected)?;
            let id = active
                .connection_instance_id
                .ok_or(SendRejected::NotConnected)?;
            let permit = if frame.binary.is_empty() {
                None
            } else {
                if active.audio_pending.get() >= 8 {
                    return Err(SendRejected::QueueFull);
                }
                active.audio_pending.set(active.audio_pending.get() + 1);
                Some(AudioPermit(active.audio_pending.clone()))
            };
            let tx = active.tx.clone();
            let submission = LocalSubmissionId(inner.next_local_submission_id);
            inner.next_local_submission_id = inner.next_local_submission_id.wrapping_add(1);
            (tx, id, submission, permit)
        };
        tx.try_send(ConnectionCommand::SendFrame {
            frame,
            connection_instance_id,
            local_submission_id,
            permit,
        })
        .map_err(|error| match error {
            mpsc::error::TrySendError::Full(_) => SendRejected::QueueFull,
            mpsc::error::TrySendError::Closed(_) => SendRejected::ConnectionClosed,
        })
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
                audio_pending: Rc::new(Cell::new(0)),
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
            #[cfg(target_arch = "wasm32")]
            Self::Invalidated(ConnectionInvalidation::ForegroundResume { .. }) => {
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
    if connect_error_invalidates_credentials(error) {
        log::warn!(
            "MQTT connection to {local_host_id} lost broker authorization ({error}); discarding \
             cached credentials so the retry mints a fresh grant"
        );
        super::service::clear_cached_credentials(local_host_id);
    }
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

/// A frame read owns the reader for its whole duration and hands it back on
/// completion, exactly like [`CompletedWrite`] does for the writer. A TYD2
/// record read is a multi-step `read_exact` sequence; if its future were
/// recreated per `select!` iteration, any other branch completing mid-record
/// (a finished write, a control change) would drop it after it had already
/// consumed part of the record, desyncing the byte stream and failing the
/// next read with "invalid TYD2 record magic".
struct CompletedRead<R> {
    frames: protocol::FrameReader<R>,
    result: Result<Option<protocol::ProtocolFrame>, protocol::FrameError>,
}

type PendingRead<S> =
    Pin<Box<dyn Future<Output = CompletedRead<BufReader<tokio::io::ReadHalf<S>>>>>>;

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
    let mut frames_slot = Some(protocol::FrameReader::new(BufReader::new(read_half)));
    let mut read_future: Option<PendingRead<S>> = None;
    let mut write_half = Some(write_half);
    let mut in_flight = None;
    let mut write_future: Option<PendingWrite<S>> = None;

    loop {
        // The in-progress frame read is retained across select iterations (see
        // [`CompletedRead`]); it is only recreated after the previous read
        // completed and parked the reader back in `frames_slot`.
        if read_future.is_none() {
            let mut frames = frames_slot
                .take()
                .expect("frame reader is parked whenever no read is in flight");
            read_future = Some(Box::pin(async move {
                let result = frames.read_frame().await;
                CompletedRead { frames, result }
            }));
        }
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
            read_completed = async {
                match read_future.as_mut() {
                    Some(future) => future.await,
                    None => std::future::pending().await,
                }
            } => {
                let CompletedRead { frames, result } = read_completed;
                read_future = None;
                frames_slot = Some(frames);
                match result {
                    Ok(None) => {
                        settle_connected_teardown(local_host_id, in_flight.take(), rx);
                        return ConnectedOutcome::Disconnected(ConnectErr::Io(std::io::Error::new(
                            std::io::ErrorKind::UnexpectedEof,
                            "MQTT Tyde byte stream closed",
                        )));
                    }
                    Ok(Some(frame)) => {
                        if frame.binary.is_empty() {
                            let line = match serde_json::to_string(&frame.envelope) {
                                Ok(line) => line,
                                Err(error) => {
                                    settle_connected_teardown(local_host_id, in_flight.take(), rx);
                                    return ConnectedOutcome::Disconnected(ConnectErr::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, error)));
                                }
                            };
                            manager.emit_host_line(
                                local_host_id,
                                actor_instance_id,
                                connection_instance_id,
                                line,
                            );
                        } else if let Err(error) = crate::voice::handle_binary_frame(frame) {
                            settle_connected_teardown(local_host_id, in_flight.take(), rx);
                            return ConnectedOutcome::Disconnected(ConnectErr::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, error)));
                        }
                    }
                    Err(error) => {
                        settle_connected_teardown(local_host_id, in_flight.take(), rx);
                        // A transport failure (e.g. the planned renewal-deadline
                        // teardown) reaches here as `FrameError::Io`; keep the
                        // original io error so retryability and credential
                        // classification can still see the typed error inside.
                        // Genuine framing corruption keeps `InvalidData`.
                        let io_error = match error {
                            protocol::FrameError::Io(io_error) => io_error,
                            other => std::io::Error::new(std::io::ErrorKind::InvalidData, other),
                        };
                        return ConnectedOutcome::Disconnected(ConnectErr::Io(io_error));
                    }
                }
            }
            command = rx.recv(), if write_future.is_none() => {
                let Some(command) = command else {
                    settle_connected_teardown(local_host_id, None, rx);
                    return ConnectedOutcome::StopRequested;
                };
                let (submission_connection_id,local_submission_id)=match &command { ConnectionCommand::SendLine{connection_instance_id,local_submission_id,..}|ConnectionCommand::SendFrame{connection_instance_id,local_submission_id,..}=>(*connection_instance_id,*local_submission_id) };
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
                    let write = async { match command { ConnectionCommand::SendLine{line,..}=>write_host_line(&mut writer,&line).await, ConnectionCommand::SendFrame{frame,permit,..}=>{let result=protocol::write_frame(&mut writer,&frame).await.map_err(|error|std::io::Error::new(std::io::ErrorKind::InvalidData,error));drop(permit);result} } };
                    let result = match timeout(writer_deadline, write).await {
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
    while let Ok(command) = rx.try_recv() {
        let (connection_instance_id, local_submission_id) = match command {
            ConnectionCommand::SendLine {
                connection_instance_id,
                local_submission_id,
                ..
            }
            | ConnectionCommand::SendFrame {
                connection_instance_id,
                local_submission_id,
                ..
            } => (connection_instance_id, local_submission_id),
        };
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
    let envelope: protocol::Envelope = serde_json::from_str(line)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    protocol::write_envelope(writer, &envelope)
        .await
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
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
    if let Some(write_ack) = write_ack_error_from_io(error) {
        return write_ack.is_retryable();
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

/// Typed transport errors can sit several layers deep: the byte stream fails
/// with `io::Error(MqttTransportError)`, the frame reader wraps that in
/// `FrameError::Io`, and callers may wrap once more. A single-level
/// `get_ref()` downcast misses those, which misclassified the planned
/// renewal-deadline teardown as fatal — so walk the whole source chain.
fn error_source_from_io<T: std::error::Error + 'static>(error: &std::io::Error) -> Option<&T> {
    let mut source: Option<&(dyn std::error::Error + 'static)> = error
        .get_ref()
        .map(|inner| inner as &(dyn std::error::Error + 'static));
    while let Some(current) = source {
        if let Some(typed) = current.downcast_ref::<T>() {
            return Some(typed);
        }
        // `io::Error::source()` skips the payload it carries (it returns the
        // payload's own source), so descend through nested io errors via
        // `get_ref` to keep each payload visible to the downcast above.
        source = match current.downcast_ref::<std::io::Error>() {
            Some(nested_io) => nested_io
                .get_ref()
                .map(|inner| inner as &(dyn std::error::Error + 'static)),
            None => current.source(),
        };
    }
    None
}

fn transport_error_from_io(error: &std::io::Error) -> Option<&MqttTransportError> {
    error_source_from_io::<MqttTransportError>(error)
}

/// Outbound write failures cross the io boundary as [`mqtt_transport::WriteAckError`]
/// (the typed transport error itself is not cloneable across every ack); both
/// carriers preserve retryability and credential classification.
fn write_ack_error_from_io(error: &std::io::Error) -> Option<&mqtt_transport::WriteAckError> {
    error_source_from_io::<mqtt_transport::WriteAckError>(error)
}

/// True when the failure means the broker rejected this connection's grant
/// (AWS IoT re-validates the CONNECT token via its custom authorizer roughly
/// every 5 minutes) or the client's own renewal deadline passed. Reconnecting
/// with the cached grant would just fail again, so the cache must be dropped
/// before the retry mints credentials.
fn connect_error_invalidates_credentials(error: &ConnectErr) -> bool {
    match error {
        ConnectErr::Transport(transport) => transport.invalidates_managed_credentials(),
        ConnectErr::Io(io_error) => transport_error_from_io(io_error)
            .map(MqttTransportError::invalidates_managed_credentials)
            .or_else(|| {
                write_ack_error_from_io(io_error)
                    .map(mqtt_transport::WriteAckError::invalidates_managed_credentials)
            })
            .unwrap_or(false),
        _ => false,
    }
}

fn transport_error_code(error: &MqttTransportError) -> MobileAccessErrorCode {
    match error {
        MqttTransportError::Configuration { .. } => MobileAccessErrorCode::InvalidConfig,
        MqttTransportError::BrokerConnect { .. }
        | MqttTransportError::Subscribe { .. }
        | MqttTransportError::SubscribeRejected { .. }
        | MqttTransportError::BrokerDisconnected { .. }
        | MqttTransportError::PeerSilenceTimeout { .. }
        | MqttTransportError::ManagedSessionExpired => {
            MobileAccessErrorCode::BrokerConnectionFailed
        }
        MqttTransportError::Publish { .. } | MqttTransportError::PublishRejected { .. } => {
            MobileAccessErrorCode::BrokerProtocol
        }
        MqttTransportError::Framing(_)
        | MqttTransportError::RetainedMessage { .. }
        | MqttTransportError::PublishAckMismatch { .. }
        | MqttTransportError::PublishAckTimeout { .. }
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
