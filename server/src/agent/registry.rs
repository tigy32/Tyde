use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use protocol::{
    AgentActivity, AgentControlStatus, AgentErrorCode, AgentId, AgentOrigin, AgentStartPayload,
    AgentWorkflowMetadata, BackendAccessMode, BackendKind, CustomAgentId, ProjectId,
    SendMessagePayload, SessionId, SessionSettingsSchema, SessionSettingsValues, SpawnCostHint,
    TeamId, TeamMemberId,
};
use tokio::sync::{Mutex, broadcast, oneshot, watch};
use uuid::Uuid;

use crate::agent::customization::ResolvedSpawnConfig;
use crate::agent::{
    AgentActorRuntimeResources, AgentHandle, RelayAgentRuntimeResources, RelayEventReceivers,
    now_ms, spawn_agent_actor, spawn_relay_agent_actor,
};
use crate::agent_control_mcp::{
    AGENT_CONTROL_AWAIT_MCP_SERVER_NAME, AGENT_CONTROL_MCP_SERVER_NAME, AgentControlMcpHandle,
};
use crate::host::mcp_url_for_agent;
use crate::review_mcp::REVIEW_FEEDBACK_MCP_SERVER_NAME;
use crate::workflows::mcp::WORKFLOW_PROGRESS_MCP_SERVER_NAME;
use protocol::McpTransportConfig;

/// Bounded so a stalled consumer cannot grow the queue without limit. A
/// consumer that overruns it sees `Lagged` and reports the gap.
const AGENT_STATUS_TRANSITION_CAPACITY: usize = 256;

pub(crate) struct AgentRegistry {
    agents: HashMap<AgentId, AgentEntry>,
    status_change_tx: watch::Sender<u64>,
    status_change_counter: Arc<AtomicU64>,
    transition_tx: broadcast::Sender<AgentStatusTransition>,
}

/// An agent crossing from one `AgentControlStatus` to another, or its
/// liveness (`AgentStatus::is_active`) flipping while the status holds — an
/// unanswered async question reports `Thinking` while work continues and
/// `AwaitingUser` once the turn ends.
/// The registry owns the status, so it computes the edge where the status is
/// mutated; consumers that need edges rather than levels subscribe here
/// instead of mirroring every agent's last known status.
#[derive(Clone, Debug)]
pub(crate) struct AgentStatusTransition {
    pub agent_id: AgentId,
    pub from: AgentControlStatus,
    pub goal: Option<protocol::NativeGoal>,
    pub goal_status_changed: bool,
    pub to: AgentControlStatus,
    pub pending_user_response: Option<PendingUserResponseKind>,
    pub has_queued_messages: bool,
    pub has_background_work: bool,
    pub restored_without_live_turn: bool,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct AgentStatus {
    pub started: bool,
    pub terminated: bool,
    pub is_thinking: bool,
    pub turn_completed: bool,
    pub goal: Option<protocol::NativeGoal>,
    pub goal_capabilities: Option<protocol::GoalCapabilities>,
    pub pending_user_response: Option<PendingUserResponseKind>,
    /// Derived by the actor from unanswered canonical requests, not card presence.
    pub blocked_on_user_response: bool,
    pub last_error: Option<String>,
    pub activity_counter: u64,
    /// This agent's transcript was replayed from a saved session and no live
    /// turn has begun since. The supervisor reads it to leave reopened history
    /// alone until the agent actually works, unless the host opts into
    /// supervising restored agents.
    pub restored_without_live_turn: bool,
    /// The agent has messages queued behind the current turn, so reaching
    /// `Idle` means "between turns", not "finished". Maintained wherever the
    /// queue snapshot is published.
    pub has_queued_messages: bool,
    pub has_background_work: bool,
    /// When the current or most recent live turn started. The supervisor's
    /// stall clock starts here, so a turn whose backend never emits anything is
    /// still measured from the moment it began rather than from an older event.
    pub turn_started_at: Option<std::time::Instant>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PendingUserResponseKind {
    UserQuestion,
    PlanApproval,
}

impl AgentStatus {
    pub fn is_active(&self) -> bool {
        !self.terminated && (!self.started || self.is_thinking || !self.turn_completed)
    }

    /// Activity published to clients. A turn blocked on the user's answer
    /// stays open but has stopped typing, so it is never `Thinking`. An
    /// unanswered async question does not hold the turn: while work continues
    /// the agent is `Thinking`, and once the turn ends the user's answer is
    /// what the agent is waiting for.
    pub fn activity(&self) -> AgentActivity {
        if self.terminated {
            AgentActivity::Idle
        } else if self.blocked_on_user_response {
            AgentActivity::AwaitingUser
        } else if self.is_active() {
            AgentActivity::Thinking
        } else if self.pending_user_response.is_some() {
            AgentActivity::AwaitingUser
        } else {
            AgentActivity::Idle
        }
    }

    pub fn is_user_response_pending(&self) -> bool {
        self.pending_user_response.is_some()
    }

    pub fn status(&self) -> AgentControlStatus {
        if self.terminated && self.last_error.is_some() {
            return AgentControlStatus::Failed;
        }
        match self.activity() {
            AgentActivity::Idle => AgentControlStatus::Idle,
            AgentActivity::Thinking => AgentControlStatus::Thinking,
            AgentActivity::AwaitingUser => AgentControlStatus::AwaitingUser,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ContinuationState {
    NotRequired,
    Pending,
    Delivered,
    Abandoned,
}

/// One restored agent's place in host restart recovery. The host activates
/// the agent only after every restored child has settled, and the agent
/// settles once its replay boundary has made its single continuation
/// attempt — or it failed, timed out, or left — so one held subtree never
/// stalls another. When a restored parent waits on this agent, startup after
/// activation is bounded by `startup_timeout`, so a hung startup fails instead
/// of holding its parent.
pub(crate) struct RestorationAdmission {
    pub agent_id: Option<AgentId>,
    pub startup_timeout: Option<std::time::Duration>,
    activation: tokio_util::sync::CancellationToken,
    settled: watch::Sender<bool>,
    continuation: std::sync::Mutex<ContinuationState>,
    restored_children: std::sync::Mutex<Vec<AgentId>>,
}

impl RestorationAdmission {
    pub fn new(
        agent_id: Option<AgentId>,
        startup_timeout: Option<std::time::Duration>,
    ) -> Arc<Self> {
        Arc::new(Self {
            agent_id,
            startup_timeout,
            activation: tokio_util::sync::CancellationToken::new(),
            settled: watch::channel(false).0,
            continuation: std::sync::Mutex::new(ContinuationState::NotRequired),
            restored_children: std::sync::Mutex::new(Vec::new()),
        })
    }

    pub fn record_restored_child(&self, child: AgentId) {
        self.restored_children
            .lock()
            .expect("restored children mutex")
            .push(child);
    }

    pub fn restored_children(&self) -> Vec<AgentId> {
        self.restored_children
            .lock()
            .expect("restored children mutex")
            .clone()
    }

    pub fn activate(&self) {
        self.activation.cancel();
    }

    pub async fn wait_for_activation(&self) {
        self.activation.cancelled().await;
    }

    pub async fn wait_until_settled(&self) {
        let mut settled = self.settled.subscribe();
        while !*settled.borrow_and_update() {
            if settled.changed().await.is_err() {
                return;
            }
        }
    }

    fn settle(&self) {
        self.settled.send_replace(true);
    }

    /// The host could not reconstruct this agent, so nothing will settle it.
    pub fn abandon(&self) {
        self.resolve_continuation(ContinuationState::Abandoned);
        self.settle();
    }

    fn continuation_state(&self) -> ContinuationState {
        *self.continuation.lock().expect("continuation state mutex")
    }

    fn resolve_continuation(&self, outcome: ContinuationState) {
        let mut state = self.continuation.lock().expect("continuation state mutex");
        if *state == ContinuationState::Pending {
            *state = outcome;
        }
    }
}

/// Abandons a still-pending continuation and settles the admission when the
/// actor leaves by any path, so a parent never waits on an actor that is gone.
pub(crate) struct RestorationCompletionGuard(Arc<RestorationAdmission>);

impl Drop for RestorationCompletionGuard {
    fn drop(&mut self) {
        self.0.resolve_continuation(ContinuationState::Abandoned);
        self.0.settle();
    }
}

/// The class of provider work asking to cross the restart dispatch barrier.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DispatchClass {
    /// A new user-visible turn; recorded in flight before the provider sees it.
    Turn,
    /// The host's restart continuation; the only turn allowed while it is
    /// pending.
    RestartContinuation,
    /// A tool response inside an already recorded turn.
    ToolResponse,
    /// Redirecting an already recorded turn (steering into it, or cancelling
    /// it for a queued send-now); it starts no turn of its own.
    Redirect,
    /// Host-initiated provider work (compaction) that is not a user turn.
    Internal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DispatchAdmission {
    Admitted,
    /// A restart stop has begun; no new provider work may start.
    HostStopping,
    /// The restart continuation has not been delivered yet; ordinary work
    /// must wait behind it.
    RecoveryPending,
    /// The turn could not be durably recorded in flight, so a restart could
    /// not recover it; it must not start.
    Unrecorded,
}

/// Where this actor durably records its turn recovery state and queue: its
/// session once the provider created one, and before that the startup
/// reservation of a new or forked agent.
enum RecoveryBinding {
    Session {
        store: Arc<crate::store::session::SessionStoreHandle>,
        id: SessionId,
    },
    Reservation {
        store: Arc<crate::store::session::SessionStoreHandle>,
    },
}

#[derive(Clone)]
pub(crate) struct AgentStatusHandle {
    recovery_binding: Arc<Mutex<Option<RecoveryBinding>>>,
    restoration: Arc<std::sync::OnceLock<Arc<RestorationAdmission>>>,
    restarting: Arc<std::sync::atomic::AtomicBool>,
    restart_requested: tokio_util::sync::CancellationToken,
    agent_id: AgentId,
    status: Arc<Mutex<AgentStatus>>,
    status_change_tx: watch::Sender<u64>,
    status_change_counter: Arc<AtomicU64>,
    transition_tx: broadcast::Sender<AgentStatusTransition>,
}

impl AgentStatusHandle {
    fn with_notifier(
        agent_id: AgentId,
        status_change_tx: watch::Sender<u64>,
        status_change_counter: Arc<AtomicU64>,
        transition_tx: broadcast::Sender<AgentStatusTransition>,
    ) -> Self {
        Self {
            recovery_binding: Arc::new(Mutex::new(None)),
            restoration: Arc::new(std::sync::OnceLock::new()),
            restarting: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            restart_requested: tokio_util::sync::CancellationToken::new(),
            agent_id,
            status: Arc::new(Mutex::new(AgentStatus::default())),
            status_change_tx,
            status_change_counter,
            transition_tx,
        }
    }

    pub fn bind_restoration(
        &self,
        admission: Arc<RestorationAdmission>,
        continuation: bool,
    ) -> RestorationCompletionGuard {
        *admission
            .continuation
            .lock()
            .expect("continuation state mutex") = if continuation {
            ContinuationState::Pending
        } else {
            ContinuationState::NotRequired
        };
        if self.restoration.set(Arc::clone(&admission)).is_err() {
            tracing::error!("restoration admission was bound twice");
        }
        RestorationCompletionGuard(admission)
    }

    pub fn pending_restart_continuation(&self) -> bool {
        self.restoration
            .get()
            .is_some_and(|admission| admission.continuation_state() == ContinuationState::Pending)
    }

    /// The provider accepted the restart continuation, or the resumed session
    /// was already running the interrupted turn.
    pub fn restart_continuation_delivered(&self) {
        if let Some(admission) = self.restoration.get() {
            admission.resolve_continuation(ContinuationState::Delivered);
            admission.settle();
        }
    }

    pub fn abandon_restart_continuation(&self) {
        if let Some(admission) = self.restoration.get() {
            admission.resolve_continuation(ContinuationState::Abandoned);
            admission.settle();
        }
    }

    /// The replay boundary has made its continuation attempt; the parent may
    /// activate even if this agent's continuation is still held.
    pub fn settle_restoration(&self) {
        if let Some(admission) = self.restoration.get() {
            admission.settle();
        }
    }

    pub fn restored_children(&self) -> Vec<AgentId> {
        self.restoration
            .get()
            .map(|admission| admission.restored_children())
            .unwrap_or_default()
    }

    /// Records the session this actor persists recovery to.
    pub async fn bind_recovery(
        &self,
        store: Arc<crate::store::session::SessionStoreHandle>,
        id: SessionId,
    ) {
        *self.recovery_binding.lock().await = Some(RecoveryBinding::Session { store, id });
    }

    /// Durably reserves a new or forked agent before anything is admitted to
    /// its provider, and returns the messages an earlier reservation of the
    /// same agent still holds.
    pub async fn reserve_startup(
        &self,
        store: Arc<crate::store::session::SessionStoreHandle>,
        reservation: crate::store::session::StartupReservation,
    ) -> Result<Vec<protocol::QueuedMessageEntry>, String> {
        let mut binding = self.recovery_binding.lock().await;
        let queued = store.reserve_startup(reservation).await?;
        *binding = Some(RecoveryBinding::Reservation { store });
        Ok(queued)
    }

    /// Moves the startup reservation's recovery state onto the session the
    /// provider created, under the barrier so a concurrent restart stop
    /// converts exactly one of them. If the move cannot be recorded the
    /// reservation stays authoritative.
    pub async fn promote_reservation(
        &self,
        store: Arc<crate::store::session::SessionStoreHandle>,
        id: SessionId,
        restore_state: Option<crate::store::session::SessionRestoreState>,
    ) {
        let mut binding = self.recovery_binding.lock().await;
        if !matches!(binding.as_ref(), Some(RecoveryBinding::Reservation { .. })) {
            if let Some(restore_state) = restore_state
                && let Err(error) = store.set_restore_state(&id, restore_state).await
            {
                tracing::error!(%error, "cannot record the session's restore state");
            }
            *binding = Some(RecoveryBinding::Session { store, id });
            return;
        }
        if let Err(error) = store
            .promote_startup_reservation(&self.agent_id, &id, restore_state)
            .await
        {
            tracing::error!(%error, "cannot move the startup reservation onto the session");
            self.update(|status| {
                status.last_error = Some(format!("Cannot record the agent's session: {error}"))
            })
            .await;
            return;
        }
        *binding = Some(RecoveryBinding::Session { store, id });
    }

    /// Persists the durable queue where a restart restores it from.
    pub async fn persist_queued_messages(
        &self,
        messages: Vec<protocol::QueuedMessageEntry>,
    ) -> Result<(), String> {
        let binding = self.recovery_binding.lock().await;
        let result = match binding.as_ref() {
            Some(RecoveryBinding::Session { store, id }) => {
                store
                    .update(id, move |record| record.queued_messages = messages)
                    .await
            }
            Some(RecoveryBinding::Reservation { store }) => {
                store
                    .update_startup_reservation(&self.agent_id, move |reservation| {
                        reservation.queued_messages = messages
                    })
                    .await
            }
            None => Err("the agent has no durable session or startup reservation".to_owned()),
        };
        if let Err(error) = &result {
            tracing::error!(%error, "cannot persist queued messages");
        }
        result
    }

    /// An explicit close wins over a restart in progress: the agent must not
    /// be continued or reconstructed, whatever the stop already recorded.
    pub async fn withdraw_recovery(&self) {
        let mut binding = self.recovery_binding.lock().await;
        let result = match binding.as_ref() {
            Some(RecoveryBinding::Session { store, id }) => {
                store.update(id, |record| record.turn_recovery = None).await
            }
            Some(RecoveryBinding::Reservation { store }) => {
                let result = store.remove_startup_reservation(&self.agent_id).await;
                if result.is_ok() {
                    *binding = None;
                }
                result
            }
            None => Ok(()),
        };
        if let Err(error) = result {
            tracing::error!(%error, "cannot withdraw turn recovery state");
        }
    }

    /// The single restart dispatch barrier. Every path that hands work to the
    /// provider crosses it, serialized with `prepare_restart`, so a turn is
    /// either recorded in flight before the stop converts it, or refused.
    pub async fn admit_dispatch(&self, class: DispatchClass) -> DispatchAdmission {
        let binding = self.recovery_binding.lock().await;
        if self.restarting() {
            return DispatchAdmission::HostStopping;
        }
        if matches!(
            class,
            DispatchClass::Turn | DispatchClass::Internal | DispatchClass::Redirect
        ) && self.pending_restart_continuation()
        {
            return DispatchAdmission::RecoveryPending;
        }
        if matches!(
            class,
            DispatchClass::Turn | DispatchClass::RestartContinuation
        ) && self
            .write_recovery(
                &binding,
                Some(crate::store::session::TurnRecovery::InFlight),
            )
            .await
            .is_err()
        {
            return DispatchAdmission::Unrecorded;
        }
        DispatchAdmission::Admitted
    }

    pub async fn persist_recovery(&self, marker: Option<crate::store::session::TurnRecovery>) {
        let binding = self.recovery_binding.lock().await;
        if self.restarting()
            && marker != Some(crate::store::session::TurnRecovery::InterruptedByRestart)
        {
            return;
        }
        let _ = self.write_recovery(&binding, marker).await;
    }

    async fn write_recovery(
        &self,
        binding: &Option<RecoveryBinding>,
        marker: Option<crate::store::session::TurnRecovery>,
    ) -> Result<(), String> {
        let result = match binding.as_ref() {
            Some(RecoveryBinding::Session { store, id }) => {
                store
                    .update(id, move |record| record.turn_recovery = marker)
                    .await
            }
            Some(RecoveryBinding::Reservation { store }) => {
                store
                    .update_startup_reservation(&self.agent_id, move |reservation| {
                        reservation.turn_recovery = marker
                    })
                    .await
            }
            // Nothing is recorded, so there is no marker to clear; a turn that
            // must be recorded in flight has nowhere to go.
            None if marker.is_none() => Ok(()),
            None => Err("the agent has no durable session or startup reservation".to_owned()),
        };
        if let Err(error) = result {
            tracing::error!(%error, "cannot persist turn recovery state");
            self.update(|status| {
                status.last_error = Some(format!("Cannot persist turn recovery state: {error}"))
            })
            .await;
            return Err(error);
        }
        Ok(())
    }

    pub async fn prepare_restart(&self) {
        let binding = self.recovery_binding.lock().await;
        self.restarting.store(true, Ordering::Release);
        let result = match binding.as_ref() {
            Some(RecoveryBinding::Session { store, id }) => {
                store
                    .update(id, |record| {
                        if record.turn_recovery.is_some() {
                            record.turn_recovery =
                                Some(crate::store::session::TurnRecovery::InterruptedByRestart);
                        }
                    })
                    .await
            }
            Some(RecoveryBinding::Reservation { store }) => {
                store
                    .update_startup_reservation(&self.agent_id, |reservation| {
                        if reservation.turn_recovery.is_some() {
                            reservation.turn_recovery =
                                Some(crate::store::session::TurnRecovery::InterruptedByRestart);
                        }
                    })
                    .await
            }
            None => Ok(()),
        };
        if let Err(error) = result {
            tracing::error!(%error, "cannot persist restart interruption");
            self.update(|status| {
                status.last_error = Some(format!("Cannot persist restart interruption: {error}"))
            })
            .await;
        }
        drop(binding);
        self.restart_requested.cancel();
    }

    pub async fn wait_for_restart(&self) {
        self.restart_requested.cancelled().await;
    }

    /// A restart stop has begun for this agent, so no new backend work may
    /// start even if its current turn ends before the stop command arrives.
    pub fn restarting(&self) -> bool {
        self.restarting.load(Ordering::Acquire)
    }

    pub async fn update<F>(&self, update: F)
    where
        F: FnOnce(&mut AgentStatus),
    {
        let mut status = self.status.lock().await;
        let from = status.status();
        let was_active = status.is_active();
        let had_background_work = status.has_background_work;
        let prior_goal_status = status.goal.as_ref().map(|goal| goal.status);
        update(&mut status);
        let to = status.status();
        let turn_active = status.is_active();
        let pending_user_response = status.pending_user_response;
        let has_queued_messages = status.has_queued_messages;
        let has_background_work = status.has_background_work;
        let restored_without_live_turn = status.restored_without_live_turn;
        let goal = status.goal.clone();
        let goal_status_changed = prior_goal_status.is_some()
            && prior_goal_status != goal.as_ref().map(|goal| goal.status);
        drop(status);

        if from != to
            || was_active != turn_active
            || goal_status_changed
            || had_background_work != has_background_work
        {
            // A send fails only with no live receivers, which is the normal
            // state; a receiver that falls behind learns about it from its own
            // `Lagged` error rather than from here.
            let _ = self.transition_tx.send(AgentStatusTransition {
                agent_id: self.agent_id.clone(),
                from,
                to,
                goal,
                goal_status_changed,
                pending_user_response,
                has_queued_messages,
                has_background_work,
                restored_without_live_turn,
            });
        }

        let next = self.status_change_counter.fetch_add(1, Ordering::SeqCst) + 1;
        let _ = self.status_change_tx.send(next);
    }

    pub async fn snapshot(&self) -> AgentStatus {
        self.status.lock().await.clone()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct InitialAgentAlias {
    pub name: String,
    pub persistence: InitialAgentAliasPersistence,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InitialAgentAliasPersistence {
    User,
    GeneratedIfNoUserAlias,
}

pub(crate) struct ResolvedSpawnRequest {
    pub restoration: Option<Arc<RestorationAdmission>>,
    pub name: String,
    pub origin: AgentOrigin,
    pub custom_agent_id: Option<CustomAgentId>,
    pub team_id: Option<TeamId>,
    pub team_member_id: Option<TeamMemberId>,
    pub workflow: Option<AgentWorkflowMetadata>,
    pub parent_agent_id: Option<AgentId>,
    pub parent_session_id: Option<SessionId>,
    pub project_id: Option<ProjectId>,
    pub backend_kind: BackendKind,
    pub launch_profile_id: Option<protocol::LaunchProfileId>,
    pub workspace_roots: Vec<String>,
    pub initial_input: Option<SendMessagePayload>,
    pub cost_hint: Option<SpawnCostHint>,
    pub session_settings: Option<SessionSettingsValues>,
    pub session_settings_schema: Option<SessionSettingsSchema>,
    pub backend_config: protocol::BackendConfigValues,
    /// Which ACP agent to launch, resolved from `launch_profile_id`. Only set
    /// for [`BackendKind::Kiro`].
    pub acp_agent: Option<protocol::AcpAgentSpec>,
    pub resolved_spawn_config: ResolvedSpawnConfig,
    pub resume_session_id: Option<SessionId>,
    pub fork_from_session_id: Option<SessionId>,
    pub startup_warning: Option<String>,
    pub startup_failure: Option<AgentStartupFailure>,
    pub initial_alias: Option<InitialAgentAlias>,
    /// When true, all backend spawns use MockBackend regardless of backend_kind.
    /// Set by the test fixture.
    pub use_mock_backend: bool,
    /// Test-only behavior reserved for this mock backend launch.
    pub mock_launch: Option<crate::backend::mock::MockLaunch>,
}

#[derive(Clone, Debug)]
pub(crate) struct AgentStartupFailure {
    pub code: AgentErrorCode,
    pub message: String,
}

impl AgentStartupFailure {
    pub fn backend_failed(message: impl Into<String>) -> Self {
        Self {
            code: AgentErrorCode::BackendFailed,
            message: message.into(),
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            code: AgentErrorCode::Internal,
            message: message.into(),
        }
    }

    pub fn unsupported(message: impl Into<String>) -> Self {
        Self {
            code: AgentErrorCode::Unsupported,
            message: message.into(),
        }
    }
}

pub(crate) struct RelaySpawnRequest {
    pub name: String,
    pub origin: AgentOrigin,
    pub custom_agent_id: Option<CustomAgentId>,
    pub parent_agent_id: AgentId,
    pub project_id: Option<ProjectId>,
    pub backend_kind: BackendKind,
    pub workspace_roots: Vec<String>,
    pub session_id: SessionId,
    pub workflow: Option<AgentWorkflowMetadata>,
}

pub(crate) struct SpawnedAgent {
    pub start: AgentStartPayload,
    pub handle: AgentHandle,
    pub startup_rx: oneshot::Receiver<Result<SessionId, String>>,
}

pub(crate) struct SpawnedRelayAgent {
    pub start: AgentStartPayload,
    pub handle: AgentHandle,
}

struct AgentEntry {
    handle: AgentHandle,
    status_handle: AgentStatusHandle,
    access_mode: BackendAccessMode,
    parent_agent_id: Option<AgentId>,
    // A completed review still has a live chat that can reread its snapshot.
    review_context: Option<tempfile::TempDir>,
}

impl AgentRegistry {
    pub fn new() -> Self {
        let (status_change_tx, _status_change_rx) = watch::channel(0);
        let (transition_tx, _transition_rx) = broadcast::channel(AGENT_STATUS_TRANSITION_CAPACITY);
        Self {
            agents: HashMap::new(),
            status_change_tx,
            status_change_counter: Arc::new(AtomicU64::new(0)),
            transition_tx,
        }
    }

    pub fn spawn(
        &mut self,
        mut request: ResolvedSpawnRequest,
        agent_control_mcp: &AgentControlMcpHandle,
        runtime: AgentActorRuntimeResources,
    ) -> SpawnedAgent {
        let agent_id = request
            .restoration
            .as_ref()
            .and_then(|admission| admission.agent_id.clone())
            .unwrap_or_else(|| AgentId(Uuid::new_v4().to_string()));
        for server in &mut request.resolved_spawn_config.mcp_servers {
            if !matches!(
                server.name.as_str(),
                AGENT_CONTROL_MCP_SERVER_NAME
                    | AGENT_CONTROL_AWAIT_MCP_SERVER_NAME
                    | REVIEW_FEEDBACK_MCP_SERVER_NAME
                    | WORKFLOW_PROGRESS_MCP_SERVER_NAME
            ) {
                continue;
            }
            let McpTransportConfig::Http { url, headers, .. } = &mut server.transport else {
                panic!("Tyde injected MCP servers must use HTTP transport");
            };
            if matches!(
                server.name.as_str(),
                AGENT_CONTROL_MCP_SERVER_NAME | AGENT_CONTROL_AWAIT_MCP_SERVER_NAME
            ) {
                headers.insert(
                    axum::http::header::AUTHORIZATION.as_str().to_owned(),
                    agent_control_mcp.caller(&agent_id).authorization,
                );
            } else if matches!(
                server.name.as_str(),
                REVIEW_FEEDBACK_MCP_SERVER_NAME | WORKFLOW_PROGRESS_MCP_SERVER_NAME
            ) {
                *url = mcp_url_for_agent(url, &agent_id);
            }
        }
        let start = AgentStartPayload {
            agent_id: agent_id.clone(),
            name: request.name.clone(),
            origin: request.origin,
            backend_kind: request.backend_kind,
            launch_profile_id: request.launch_profile_id.clone(),
            workspace_roots: request.workspace_roots.clone(),
            custom_agent_id: request.custom_agent_id.clone(),
            team_id: request.team_id.clone(),
            team_member_id: request.team_member_id.clone(),
            workflow: request.workflow.clone(),
            project_id: request.project_id.clone(),
            parent_agent_id: request.parent_agent_id.clone(),
            session_id: request.resume_session_id.clone(),
            created_at_ms: now_ms(),
        };

        let access_mode = request.resolved_spawn_config.access_mode;
        let status_handle = self.next_status_handle(agent_id.clone());
        let (handle, startup_rx) = spawn_agent_actor(
            agent_id.clone(),
            start.clone(),
            request,
            runtime.with_status(status_handle.clone()),
        );

        let previous = self.agents.insert(
            agent_id.clone(),
            AgentEntry {
                handle: handle.clone(),
                status_handle,
                access_mode,
                parent_agent_id: start.parent_agent_id.clone(),
                review_context: None,
            },
        );
        assert!(
            previous.is_none(),
            "agent registry attempted to insert duplicate agent_id {}",
            agent_id
        );

        SpawnedAgent {
            start,
            handle,
            startup_rx,
        }
    }

    pub fn spawn_relay(
        &mut self,
        request: RelaySpawnRequest,
        receivers: RelayEventReceivers,
        runtime: RelayAgentRuntimeResources,
    ) -> SpawnedRelayAgent {
        let agent_id = AgentId(Uuid::new_v4().to_string());
        let start = AgentStartPayload {
            agent_id: agent_id.clone(),
            name: request.name.clone(),
            origin: request.origin,
            backend_kind: request.backend_kind,
            launch_profile_id: None,
            workspace_roots: request.workspace_roots.clone(),
            custom_agent_id: request.custom_agent_id.clone(),
            team_id: None,
            team_member_id: None,
            workflow: request.workflow.clone(),
            project_id: request.project_id.clone(),
            parent_agent_id: Some(request.parent_agent_id.clone()),
            session_id: Some(request.session_id.clone()),
            created_at_ms: now_ms(),
        };

        let status_handle = self.next_status_handle(agent_id.clone());
        let handle = spawn_relay_agent_actor(
            agent_id.clone(),
            start.clone(),
            receivers,
            runtime,
            request.session_id,
            status_handle.clone(),
        );

        let previous = self.agents.insert(
            agent_id.clone(),
            AgentEntry {
                handle: handle.clone(),
                status_handle,
                access_mode: BackendAccessMode::Unrestricted,
                parent_agent_id: start.parent_agent_id.clone(),
                review_context: None,
            },
        );
        assert!(
            previous.is_none(),
            "agent registry attempted to insert duplicate relay agent_id {}",
            agent_id
        );

        SpawnedRelayAgent { start, handle }
    }

    pub fn retain_review_context(
        &mut self,
        agent_id: &AgentId,
        context: tempfile::TempDir,
    ) -> Option<AgentHandle> {
        let entry = self.agents.get_mut(agent_id)?;
        if entry.handle.is_closing() {
            return None;
        }
        assert!(
            entry.review_context.is_none(),
            "review context already registered"
        );
        entry.review_context = Some(context);
        Some(entry.handle.clone())
    }

    pub fn take_review_context(&mut self, agent_id: &AgentId) -> Option<tempfile::TempDir> {
        self.agents.get_mut(agent_id)?.review_context.take()
    }

    pub fn remove_agent(&mut self, agent_id: &AgentId) -> Option<AgentHandle> {
        let removed = self.agents.remove(agent_id).map(|entry| entry.handle);
        if removed.is_some() {
            let next = self.status_change_counter.fetch_add(1, Ordering::SeqCst) + 1;
            let _ = self.status_change_tx.send(next);
        }
        removed
    }

    pub fn agent_handle(&self, agent_id: &AgentId) -> Option<AgentHandle> {
        self.agents.get(agent_id).map(|entry| entry.handle.clone())
    }

    pub fn agent_status_handle(&self, agent_id: &AgentId) -> Option<AgentStatusHandle> {
        self.agents
            .get(agent_id)
            .map(|entry| entry.status_handle.clone())
    }

    pub fn agent_access_mode(&self, agent_id: &AgentId) -> Option<BackendAccessMode> {
        self.agents.get(agent_id).map(|entry| entry.access_mode)
    }

    pub fn parent_agent_id(&self, agent_id: &AgentId) -> Option<AgentId> {
        self.agents.get(agent_id)?.parent_agent_id.clone()
    }

    pub async fn has_background_work(&self, agent_id: &AgentId) -> bool {
        let Some(entry) = self.agents.get(agent_id) else {
            return false;
        };
        let status = entry.status_handle.snapshot().await;
        if status.terminated {
            return false;
        }
        if status.has_background_work {
            return true;
        }
        for child in self.agents.values() {
            if child.parent_agent_id.as_ref() == Some(agent_id)
                && child.status_handle.snapshot().await.is_active()
            {
                return true;
            }
        }
        false
    }

    pub fn agent_ids(&self) -> Vec<AgentId> {
        self.agents.keys().cloned().collect()
    }

    pub fn agent_depth(&self, agent_id: &AgentId) -> Option<u8> {
        let mut current = agent_id;
        let mut depth = 1_u8;
        let mut visited = HashSet::new();
        loop {
            if !visited.insert(current.clone()) {
                return None;
            }
            let entry = self.agents.get(current)?;
            let Some(parent_agent_id) = entry.parent_agent_id.as_ref() else {
                return Some(depth);
            };
            depth = depth.checked_add(1)?;
            current = parent_agent_id;
        }
    }

    pub fn agent_subtree_post_order(&self, agent_id: &AgentId) -> Vec<(AgentId, AgentHandle)> {
        if !self.agents.contains_key(agent_id) {
            return Vec::new();
        }

        let mut children_by_parent: HashMap<AgentId, Vec<AgentId>> = HashMap::new();
        for (candidate_id, entry) in &self.agents {
            if let Some(parent_agent_id) = &entry.parent_agent_id {
                children_by_parent
                    .entry(parent_agent_id.clone())
                    .or_default()
                    .push(candidate_id.clone());
            }
        }
        for children in children_by_parent.values_mut() {
            children.sort_by(|left, right| left.0.cmp(&right.0));
        }

        let mut visited = HashSet::new();
        let mut ordered = Vec::new();
        collect_agent_subtree_post_order(
            agent_id,
            &self.agents,
            &children_by_parent,
            &mut visited,
            &mut ordered,
        );
        ordered
    }

    pub fn subscribe_status_changes(&self) -> watch::Receiver<u64> {
        self.status_change_tx.subscribe()
    }

    pub fn subscribe_status_transitions(&self) -> broadcast::Receiver<AgentStatusTransition> {
        self.transition_tx.subscribe()
    }

    fn next_status_handle(&self, agent_id: AgentId) -> AgentStatusHandle {
        AgentStatusHandle::with_notifier(
            agent_id,
            self.status_change_tx.clone(),
            Arc::clone(&self.status_change_counter),
            self.transition_tx.clone(),
        )
    }
}

fn collect_agent_subtree_post_order(
    agent_id: &AgentId,
    agents: &HashMap<AgentId, AgentEntry>,
    children_by_parent: &HashMap<AgentId, Vec<AgentId>>,
    visited: &mut HashSet<AgentId>,
    ordered: &mut Vec<(AgentId, AgentHandle)>,
) {
    if !visited.insert(agent_id.clone()) {
        return;
    }

    if let Some(children) = children_by_parent.get(agent_id) {
        for child_id in children {
            collect_agent_subtree_post_order(
                child_id,
                agents,
                children_by_parent,
                visited,
                ordered,
            );
        }
    }

    if let Some(entry) = agents.get(agent_id) {
        ordered.push((agent_id.clone(), entry.handle.clone()));
    }
}
