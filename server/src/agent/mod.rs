use std::collections::{HashMap, HashSet, VecDeque};
use std::ops::{Deref, DerefMut};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use protocol::{
    AgentActivityStats, AgentActivityStatsPayload, AgentActivitySummary, AgentBootstrapEvent,
    AgentBootstrapPayload, AgentControlLatestOutput, AgentControlOutput, AgentErrorCode,
    AgentErrorPayload, AgentId, AgentInput, AgentOrigin, AgentRenamedPayload, AgentStartPayload,
    BackendAccessMode, BackendKind, ChatEvent, ChatMessage, ChatMessageId, CompactionMethod,
    CompactionMetrics, CompactionMutation, CompactionObservationId, CompactionOperationId,
    CompactionStage, CompactionTrigger, ContextBreakdown, ContextCompactionCapabilityPayload,
    ContextCompactionNotifyPayload, ContextCompactionStatus, ContextCompactionTimelineEvent,
    ContextCompactionTimelineStatus, Envelope, FrameKind, MessageMetadataUpdateData, MessageOrigin,
    MessageSender, MessageTokenUsage, ModelRequestId, ModelRequestTokenUsage, QueuedMessageEntry,
    QueuedMessageId, QueuedMessagesPayload, ReviewErrorContext, SUPERVISOR_MESSAGE_PREFIX,
    SendMessagePayload, SessionId, SessionSettingsPayload, SessionSettingsValues,
    SessionSummaryCountUpdatedPayload, SpawnCostHint, StreamEndData, StreamStartData,
    StreamTextDeltaData, TaskTokenUsageAmount, TaskTokenUsageScope,
    TaskTokenUsageUnavailableReason, TokenUsage, TokenUsageScope, TokenUsageUnavailableReason,
    ToolExecutionCompletedData, ToolExecutionMode, ToolExecutionOutcome, ToolExecutionResult,
    ToolPolicy, ToolRequestType,
};
use tokio::sync::{Mutex, mpsc, oneshot, watch};
use uuid::Uuid;

use crate::backend::antigravity::AntigravityBackend;
use crate::backend::antigravity::is_antigravity_native_session_id;
use crate::backend::claude::ClaudeBackend;
use crate::backend::codex::CodexBackend;
use crate::backend::hermes::HermesBackend;
use crate::backend::kiro::KiroBackend;
use crate::backend::mock::MockBackend;
use crate::backend::tycode::TycodeBackend;
use crate::backend::{
    Backend, BackendEvent, BackendExecutionMode, BackendSession, BackendSpawnConfig,
    BackendStartupError, EventStream, SendOutcome, apply_session_settings_update,
    resolve_backend_session_settings, validate_runtime_session_settings_update,
    validate_session_settings_values, validate_startup_mcp_configuration,
};
use crate::host::{
    HostCapacityTx, HostSessionSummaryCountEvent, HostSessionSummaryCountTx,
    HostSessionSummaryCountUpdate, HostSubAgentEmitter,
};
use crate::review::ReviewRegistryHandle;
use crate::store::session::{
    CommitCompactedBinding, CompactionOperationRecord, FinishCompactionOperation, SessionStore,
    StoredCompactionState,
};
use crate::store::transcript::TranscriptStore;
use crate::stream::Stream;
use crate::sub_agent::HostSubAgentSpawnTx;

pub(crate) mod customization;
pub(crate) mod registry;
pub(crate) mod supervisor;

use self::registry::{
    AgentStartupFailure, InitialAgentAlias, InitialAgentAliasPersistence, ResolvedSpawnRequest,
};

const IMAGE_ONLY_AGENT_NAME: &str = "Image Review Task";
const RESUME_REPLAY_BARRIER_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a close waits for an interrupted turn to reach idle before it
/// tears the actor down anyway.
///
/// Closing a busy agent interrupts its turn, and a healthy backend reports
/// idle well inside this window. The deadline exists for the backends that
/// never answer: without it a close waits on an idle transition that may never
/// arrive, which parks the actor in [`ActorLifecycle::Closing`] forever and
/// leaves an agent the user cannot cancel, close, or message.
const CLOSE_TURN_GRACE: Duration = Duration::from_secs(10);
const INITIAL_HISTORY_TAIL_LIMIT: usize = 15;
pub(crate) const DEFAULT_COMPACTION_SUMMARY_MAX_BYTES: usize = 32 * 1024;
pub(crate) const MAX_COMPACTION_SUMMARY_BYTES: usize = 128 * 1024;
const COMPACTION_RETRY_INITIAL_DELAY: Duration = Duration::from_millis(100);
const COMPACTION_RETRY_MAX_DELAY: Duration = Duration::from_secs(5);

type BackendHandle = Box<dyn BackendSender>;
type BackendSpawnResult = Result<(BackendHandle, EventStream, SessionId), String>;
type BackendForkResult = Result<(BackendHandle, EventStream, SessionId), BackendStartupError>;
type BackendResumeResult = Result<(BackendHandle, EventStream), String>;
type BackendFuture<T> = Pin<Box<dyn std::future::Future<Output = T> + Send>>;

#[derive(Clone)]
struct HostSubAgentEmitterContext {
    host_sub_agent_spawn_tx: HostSubAgentSpawnTx,
    capacity_tx: HostCapacityTx,
}

impl HostSubAgentEmitterContext {
    fn emitter(self, agent_id: AgentId, workspace_roots: Vec<String>) -> HostSubAgentEmitter {
        HostSubAgentEmitter::new(
            self.host_sub_agent_spawn_tx,
            self.capacity_tx,
            agent_id,
            workspace_roots,
        )
    }
}

impl From<BackendStartupError> for AgentStartupFailure {
    fn from(error: BackendStartupError) -> Self {
        Self {
            code: error.code,
            message: error.message,
        }
    }
}

struct TerminalFailureContext<'a> {
    accepting_input: &'a Arc<AtomicBool>,
    status_handle: &'a registry::AgentStatusHandle,
    canonical_stream: &'a str,
    event_log: &'a mut Vec<Envelope>,
    replay_state: &'a mut AgentReplayState,
    subscribers: &'a mut Vec<Stream>,
    queue: &'a mut VecDeque<SequencedQueuedMessage>,
    session_store: &'a Arc<Mutex<SessionStore>>,
    compaction: Option<TerminalCompactionFailureContext<'a>>,
}

struct TerminalCompactionFailureContext<'a> {
    flight: &'a mut Option<CompactionFlight>,
    session_store: &'a Arc<Mutex<SessionStore>>,
    session_id: &'a SessionId,
    start: &'a AgentStartPayload,
    activity_stats: &'a mut AgentActivityStatsTracker,
}

struct InitialFollowUpContext<'a> {
    backend: &'a mut Option<BackendHandle>,
    in_turn: &'a mut bool,
    idle_transition_armed: &'a mut bool,
    session_store: &'a Arc<Mutex<SessionStore>>,
    transcript_store: &'a TranscriptStore,
    current_session_id: Option<&'a SessionId>,
    pending_alias: &'a mut Option<InitialAgentAlias>,
    current_start: &'a mut AgentStartPayload,
    start_tx: &'a watch::Sender<AgentStartPayload>,
    accepting_input: &'a Arc<AtomicBool>,
    status_handle: &'a registry::AgentStatusHandle,
    canonical_stream: &'a str,
    event_log: &'a mut Vec<Envelope>,
    latest_output: &'a mut AgentControlLatestOutput,
    replay_state: &'a mut AgentReplayState,
    subscribers: &'a mut Vec<Stream>,
    queue: &'a mut VecDeque<SequencedQueuedMessage>,
    next_queue_sequence: &'a mut u64,
    pending_inputs: &'a mut VecDeque<AgentInput>,
    rx: &'a mut mpsc::UnboundedReceiver<AgentCommand>,
}

struct QueueDispatchTerminalContext<'a> {
    accepting_input: &'a Arc<AtomicBool>,
    status_handle: &'a registry::AgentStatusHandle,
    canonical_stream: &'a str,
    event_log: &'a mut Vec<Envelope>,
    replay_state: &'a mut AgentReplayState,
    subscribers: &'a mut Vec<Stream>,
    queue: &'a mut VecDeque<SequencedQueuedMessage>,
    session_store: &'a Arc<Mutex<SessionStore>>,
    transcript_store: &'a TranscriptStore,
    context_compaction: &'a mut Option<CompactionFlight>,
    activity_stats: &'a mut AgentActivityStatsTracker,
    current_session_id: Option<&'a SessionId>,
    pending_alias: &'a mut Option<InitialAgentAlias>,
    current_start: &'a mut AgentStartPayload,
    start_tx: &'a watch::Sender<AgentStartPayload>,
    latest_output: &'a mut AgentControlLatestOutput,
    pending_inputs: &'a mut VecDeque<AgentInput>,
    rx: &'a mut mpsc::UnboundedReceiver<AgentCommand>,
    open_tool_call_ids: &'a mut HashSet<String>,
    pending_tool_response_ids: &'a mut HashSet<String>,
    active_agent_await_ids: &'a mut HashSet<String>,
}

struct AgentNameChangeContext<'a> {
    session_store: &'a Arc<Mutex<SessionStore>>,
    session_id: Option<&'a SessionId>,
    pending_alias: &'a mut Option<InitialAgentAlias>,
    current_start: &'a mut AgentStartPayload,
    start_tx: &'a watch::Sender<AgentStartPayload>,
    event_log: &'a mut Vec<Envelope>,
    subscribers: &'a mut Vec<Stream>,
}

pub(crate) struct AgentActorRuntimeContext {
    pub(crate) session_store: Arc<Mutex<SessionStore>>,
    pub(crate) transcript_store: TranscriptStore,
    pub(crate) host_sub_agent_spawn_tx: HostSubAgentSpawnTx,
    pub(crate) capacity_tx: HostCapacityTx,
    pub(crate) session_summary_count_tx: HostSessionSummaryCountTx,
    pub(crate) review_registry: ReviewRegistryHandle,
    pub(crate) status_handle: registry::AgentStatusHandle,
    pub(crate) supervisor_settings_rx: watch::Receiver<crate::host::SupervisorSettingsSignal>,
    pub(crate) use_mock_backend: bool,
    pub(crate) supervisor_compaction_tx: crate::host::SupervisorCompactionTx,
    pub(crate) provider_version: Option<String>,
    pub(crate) antigravity_conversations_dir: PathBuf,
}

pub(crate) struct AgentActorRuntimeResources {
    pub(crate) session_store: Arc<Mutex<SessionStore>>,
    pub(crate) transcript_store: TranscriptStore,
    pub(crate) host_sub_agent_spawn_tx: HostSubAgentSpawnTx,
    pub(crate) capacity_tx: HostCapacityTx,
    pub(crate) session_summary_count_tx: HostSessionSummaryCountTx,
    pub(crate) review_registry: ReviewRegistryHandle,
    /// The supervisor runs inside the actor, so it holds the settings watch for
    /// its whole life instead of being handed one per host command.
    pub(crate) supervisor_settings_rx: watch::Receiver<crate::host::SupervisorSettingsSignal>,
    pub(crate) use_mock_backend: bool,
    pub(crate) supervisor_compaction_tx: crate::host::SupervisorCompactionTx,
    pub(crate) provider_version: Option<String>,
    pub(crate) antigravity_conversations_dir: PathBuf,
}

impl AgentActorRuntimeResources {
    pub(crate) fn with_status(
        self,
        status_handle: registry::AgentStatusHandle,
    ) -> AgentActorRuntimeContext {
        AgentActorRuntimeContext {
            session_store: self.session_store,
            transcript_store: self.transcript_store,
            host_sub_agent_spawn_tx: self.host_sub_agent_spawn_tx,
            capacity_tx: self.capacity_tx,
            session_summary_count_tx: self.session_summary_count_tx,
            review_registry: self.review_registry,
            status_handle,
            supervisor_settings_rx: self.supervisor_settings_rx,
            use_mock_backend: self.use_mock_backend,
            supervisor_compaction_tx: self.supervisor_compaction_tx,
            provider_version: self.provider_version,
            antigravity_conversations_dir: self.antigravity_conversations_dir,
        }
    }
}

enum AgentCommand {
    SendInput(AgentInput),
    /// Agent-control follow-up whose acceptance the actor acknowledges itself.
    /// See [`AgentHandle::deliver_message`] for the contract; the mailbox
    /// accepting this command is deliberately *not* the commit point.
    DeliverMessage {
        payload: SendMessagePayload,
        reply: oneshot::Sender<Result<(), String>>,
    },
    Compact {
        summary_prompt: String,
        max_summary_bytes: usize,
        reply: oneshot::Sender<Result<CompactionSummary, String>>,
    },
    CompactIfInactive {
        expected_activity_counter: u64,
        expected_supervisor_settings_epoch: u64,
        supervisor_settings_rx: watch::Receiver<crate::host::SupervisorSettingsSignal>,
        summary_prompt: String,
        max_summary_bytes: usize,
        accepted: oneshot::Sender<Result<(), String>>,
        reply: oneshot::Sender<Result<CompactionSummary, String>>,
    },
    ReadCompactionCapability {
        reply: oneshot::Sender<crate::backend::BackendCompactionCapability>,
    },
    ReadRequestedCompactionRoute {
        trigger: CompactionTrigger,
        reply: oneshot::Sender<Result<protocol::RequestedCompactionRoute, String>>,
    },
    RequestContextCompaction {
        trigger: CompactionTrigger,
        focus: Option<String>,
        barrier_timeout: Duration,
        inactivity_gate: Option<(
            u64,
            u64,
            watch::Receiver<crate::host::SupervisorSettingsSignal>,
        )>,
        reply: oneshot::Sender<Result<CompactionOperationId, String>>,
    },
    ContextCompactionTerminal {
        operation_id: CompactionOperationId,
        result: Result<crate::backend::BackendCompactionResult, String>,
    },
    RetryContextCompaction {
        operation_id: CompactionOperationId,
    },
    ContextCompactionFallbackPrepared {
        operation_id: CompactionOperationId,
        result: Result<PreparedContextFallback, String>,
    },
    ContextCompactionBarrierExpired {
        operation_id: CompactionOperationId,
    },
    ReleaseCompaction {
        reply: oneshot::Sender<()>,
    },
    SetName {
        name: String,
        persistence: InitialAgentAliasPersistence,
        reply: oneshot::Sender<bool>,
    },
    ApplyGeneratedName {
        result: Result<String, String>,
        reply: oneshot::Sender<bool>,
    },
    ReadOutput {
        after_seq: Option<u64>,
        limit: usize,
        reply: oneshot::Sender<Vec<Envelope>>,
    },
    ReadLatestOutput {
        reply: oneshot::Sender<Result<AgentControlOutput, String>>,
    },
    FetchSessionHistory {
        before_seq: Option<u64>,
        limit: usize,
        reply: oneshot::Sender<SessionHistoryWindow>,
    },
    ResumeReplayBarrier {
        result: Result<(), String>,
    },
    ReadActivityHistory {
        after_seq: Option<u64>,
        max_events: usize,
        max_bytes: usize,
        reply: oneshot::Sender<AgentActivityHistorySnapshot>,
    },
    ReadUsageSnapshot {
        reply: oneshot::Sender<AgentUsageSnapshot>,
    },
    Interrupt {
        reply: oneshot::Sender<InterruptOutcome>,
    },
    Close {
        reply: oneshot::Sender<()>,
    },
    #[cfg(feature = "test-support")]
    ForceBackendShutdownForConformance {
        reply: oneshot::Sender<bool>,
    },
    /// Test support: read the live mock backend's control handle, if this
    /// agent is running the scriptable mock. Answered from the actor's own
    /// `BackendHandle`, so control ownership never leaves the actor tree.
    #[cfg(feature = "test-support")]
    ReadMockControl {
        reply: oneshot::Sender<Option<crate::backend::mock::MockControl>>,
    },
    Attach {
        stream: Stream,
        reply: oneshot::Sender<bool>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CompactionSummary {
    pub session_id: SessionId,
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentActivityHistorySnapshot {
    pub rendered: String,
    pub from_seq: Option<u64>,
    pub through_seq: Option<u64>,
    pub event_count: usize,
    pub active_stream_included: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct AgentUsageSnapshot {
    pub start: AgentStartPayload,
    pub usage: TaskTokenUsageScope,
    pub model: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct SessionHistoryWindow {
    pub events: Vec<ChatEvent>,
    pub has_more_before: bool,
    pub oldest_seq: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
struct SequencedQueuedMessage {
    sequence: u64,
    entry: QueuedMessageEntry,
}

impl Deref for SequencedQueuedMessage {
    type Target = QueuedMessageEntry;

    fn deref(&self) -> &Self::Target {
        &self.entry
    }
}

impl DerefMut for SequencedQueuedMessage {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.entry
    }
}

impl SequencedQueuedMessage {
    fn into_send_payload(self) -> SendMessagePayload {
        queued_message_to_send_payload(self.entry)
    }
}

pub(crate) enum CompactionStart {
    Started(oneshot::Receiver<Result<CompactionSummary, String>>),
    Rejected(String),
    Closed,
}

struct ActiveCompaction {
    reply: oneshot::Sender<Result<CompactionSummary, String>>,
    summary: String,
    max_summary_bytes: usize,
    error: Option<String>,
}

struct CompactionFlight {
    operation_id: CompactionOperationId,
    trigger: CompactionTrigger,
    focus: Option<String>,
    queue_watermark: u64,
    state: StoredCompactionState,
    binding_generation_before: u64,
    fallback_transcript_high_water: Option<u64>,
    fallback_activity_counter: Option<u64>,
    fallback_settings: Option<SessionSettingsValues>,
    fallback_task: Option<tokio::task::JoinHandle<()>>,
    retry_armed: bool,
    retry_attempt: u8,
    method: Option<CompactionMethod>,
    provider_version: Option<String>,
    terminal_taken: bool,
}

impl CompactionFlight {
    fn admits_queue_sequence(&self, sequence: u64) -> bool {
        sequence <= self.queue_watermark
    }
}

fn context_compaction_dispatch_is_safe(
    flight: &CompactionFlight,
    queue: &VecDeque<SequencedQueuedMessage>,
    in_turn: bool,
    replay_pending: bool,
    open_tool_call_ids: &HashSet<String>,
    pending_tool_response_ids: &HashSet<String>,
    background_mutation_active: bool,
) -> bool {
    flight.state == StoredCompactionState::Deferred
        && !in_turn
        && !replay_pending
        && open_tool_call_ids.is_empty()
        && pending_tool_response_ids.is_empty()
        && !background_mutation_active
        && queue
            .front()
            .is_none_or(|queued| !flight.admits_queue_sequence(queued.sequence))
}

fn context_compaction_fallback_allowed(
    trigger: CompactionTrigger,
    capability: &crate::backend::BackendCompactionCapability,
) -> bool {
    trigger != CompactionTrigger::SupervisorRequested
        || !matches!(
            &capability.availability,
            crate::backend::BackendCompactionAvailability::AutomaticOnly { .. }
        )
}

fn backend_compaction_result_allows_inline_fallback(
    operation_id: &CompactionOperationId,
    result: &crate::backend::BackendCompactionResult,
) -> bool {
    result.operation_id == *operation_id
        && result.dispatch == crate::backend::BackendCompactionDispatchState::Rejected
        && result.mutation == crate::backend::BackendCompactionMutationState::NotObserved
        && result.outcome.is_err()
}

fn compaction_flight_can_enter_rejected_fallback(flight: &CompactionFlight) -> bool {
    flight.state == StoredCompactionState::NativeAccepted
        && flight.terminal_taken
        && flight.method != Some(CompactionMethod::InlineFallback)
        && flight.fallback_task.is_none()
}

fn inline_fallback_owns_structured_native_terminal(
    flight: &CompactionFlight,
    operation_id: &CompactionOperationId,
    result: &Result<crate::backend::BackendCompactionResult, String>,
) -> bool {
    matches!(
        flight.state,
        StoredCompactionState::FallbackPreparing | StoredCompactionState::FallbackCommitPending
    ) && flight.method == Some(CompactionMethod::InlineFallback)
        && matches!(
            result,
            Ok(result) if result.operation_id == *operation_id
        )
}

fn mark_inline_context_fallback_preparing(
    flight: &mut CompactionFlight,
    transcript_high_water: u64,
    activity_counter: u64,
    settings: SessionSettingsValues,
) {
    flight.state = StoredCompactionState::FallbackPreparing;
    flight.method = Some(CompactionMethod::InlineFallback);
    flight.terminal_taken = false;
    flight.fallback_transcript_high_water = Some(transcript_high_water);
    flight.fallback_activity_counter = Some(activity_counter);
    flight.fallback_settings = Some(settings);
}

fn arm_context_compaction_retry(
    flight: &mut CompactionFlight,
    actor_tx: &mpsc::UnboundedSender<AgentCommand>,
) {
    if flight.retry_armed {
        return;
    }
    flight.retry_armed = true;
    let delay = context_compaction_retry_delay(flight.retry_attempt);
    flight.retry_attempt = flight.retry_attempt.saturating_add(1);
    let operation_id = flight.operation_id.clone();
    let tx = actor_tx.clone();
    tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        let _ = tx.send(AgentCommand::RetryContextCompaction { operation_id });
    });
}

fn context_compaction_retry_delay(attempt: u8) -> Duration {
    let multiplier = 1_u32 << u32::from(attempt).min(16);
    COMPACTION_RETRY_INITIAL_DELAY
        .saturating_mul(multiplier)
        .min(COMPACTION_RETRY_MAX_DELAY)
}

struct PreparedContextFallback {
    binding: crate::backend::PreparedBackendBinding,
    metrics: CompactionMetrics,
}

#[derive(Default)]
struct AgentReplayState {
    active_stream: Option<ReplayActiveStream>,
    typing: bool,
    operation_cancelled: bool,
    resume_history_settled_idle: bool,
    /// Position in the event_log of the single retained `ToolProgress`
    /// envelope per tool_call_id. Progress snapshots are coalesced
    /// latest-wins (replace in place, preserving seq) so long-running
    /// background tasks don't bloat the replay log. Safe because the
    /// event_log is append-only.
    progress_log_index: HashMap<String, usize>,
    active_tool_progress: HashMap<String, protocol::ToolProgressData>,
    active_background_progress: HashMap<String, protocol::ToolProgressData>,
}

impl AgentReplayState {
    fn clear_active_stream(&mut self) {
        self.active_stream = None;
    }

    fn discard_active_stream(&mut self) {
        self.active_stream = None;
    }

    fn active_stream_events(&self) -> Vec<ChatEvent> {
        let mut events = Vec::new();
        if self.typing {
            events.push(ChatEvent::TypingStatusChanged(true));
        }

        let Some(active) = &self.active_stream else {
            return events;
        };

        events.push(ChatEvent::StreamStart(active.start.clone()));
        if !active.reasoning.is_empty() {
            events.push(ChatEvent::StreamReasoningDelta(StreamTextDeltaData {
                text: active.reasoning.clone(),
            }));
        }
        if !active.text.is_empty() {
            events.push(ChatEvent::StreamDelta(StreamTextDeltaData {
                text: active.text.clone(),
            }));
        }
        events.extend(active.tool_events.iter().cloned());
        events
    }
}

#[derive(Clone)]
struct ReplayActiveStream {
    start: StreamStartData,
    text: String,
    reasoning: String,
    tool_events: Vec<ChatEvent>,
}

#[derive(Clone)]
pub(crate) struct AgentHandle {
    tx: mpsc::UnboundedSender<AgentCommand>,
    accepting_input: Arc<AtomicBool>,
    closing: Arc<AtomicBool>,
    /// Live view of the actor's `AgentStartPayload`. Populated synchronously at
    /// handle construction and updated by the actor on name changes. Owning a
    /// clone of the receiver here means callers can snapshot the start payload
    /// without a message round-trip — which makes it structurally impossible
    /// for a stopped actor to cause the old "agent disappeared" panic.
    start: watch::Receiver<AgentStartPayload>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum TokenUsageSource {
    Message(ChatMessageId),
    EventSeq(u64),
    ModelRequest(ModelRequestId),
    PromotedRequests,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum TokenUsageTrackingMode {
    #[default]
    Messages,
    ModelRequests,
}

fn context_estimate_matches(
    estimate: &ContextBreakdown,
    current: &protocol::CurrentContextUsage,
) -> bool {
    current
        .known()
        .is_some_and(|(input_tokens, context_window)| {
            estimate.input_tokens == input_tokens && estimate.context_window == context_window
        })
}

#[derive(Debug, Default)]
struct AgentActivityStatsTracker {
    stats: AgentActivityStats,
    seen_tool_calls: HashSet<String>,
    token_usage_by_source: HashMap<TokenUsageSource, TaskTokenUsageScope>,
    authoritative_token_usage_baseline: TokenUsage,
    authoritative_token_usage_by_source: HashMap<TokenUsageSource, TokenUsage>,
    authoritative_turn_sources: HashSet<TokenUsageSource>,
    provisional_request_usage_by_source: HashMap<TokenUsageSource, TokenUsage>,
    active_reasoning: String,
    latest_model: Option<String>,
    token_usage_tracking_mode: TokenUsageTrackingMode,
}

impl AgentActivityStatsTracker {
    fn for_backend(backend_kind: BackendKind) -> Self {
        let mut tracker = Self {
            token_usage_tracking_mode: if backend_kind == BackendKind::Codex {
                TokenUsageTrackingMode::ModelRequests
            } else {
                TokenUsageTrackingMode::Messages
            },
            ..Self::default()
        };
        if backend_kind == BackendKind::Codex {
            tracker.stats.current_context_usage = Some(protocol::CurrentContextUsage::Unknown);
        }
        tracker
    }

    fn snapshot(&self) -> AgentActivityStats {
        self.stats.clone()
    }

    fn usage_snapshot(&self) -> (TaskTokenUsageScope, Option<String>) {
        self.usage_snapshot_with_reported_usage_floor(None)
    }

    fn usage_snapshot_with_reported_usage_floor(
        &self,
        reported_usage_floor: Option<&TokenUsage>,
    ) -> (TaskTokenUsageScope, Option<String>) {
        if let Some(total_tokens) = self
            .stats
            .token_usage_total_only
            .filter(|total| *total >= self.stats.token_usage.total_tokens)
        {
            return (
                TaskTokenUsageScope::Known {
                    usage: Box::new(TaskTokenUsageAmount::total_only(total_tokens)),
                },
                self.latest_model.clone(),
            );
        }
        if self.token_usage_by_source.is_empty() {
            return (
                TaskTokenUsageScope::Unavailable {
                    reason: TaskTokenUsageUnavailableReason::NoAssistantTurnCompleted,
                },
                self.latest_model.clone(),
            );
        }

        let reported_usage = reported_usage_floor
            .filter(|floor| floor.total_tokens > self.stats.token_usage.total_tokens)
            .unwrap_or(&self.stats.token_usage);
        let has_reported_usage_floor =
            reported_usage.total_tokens > self.stats.token_usage.total_tokens;
        let mut has_reported_usage = false;
        let mut partial_seen = false;
        let mut unavailable_count = 0_u32;
        let mut reasons = Vec::new();
        for usage in self.token_usage_by_source.values() {
            match usage {
                TaskTokenUsageScope::Known { .. } => {
                    has_reported_usage = true;
                }
                TaskTokenUsageScope::Partial {
                    unavailable_count: count,
                    reasons: partial_reasons,
                    ..
                } => {
                    has_reported_usage = true;
                    partial_seen = true;
                    unavailable_count = unavailable_count.saturating_add(*count);
                    extend_task_token_usage_reasons(&mut reasons, partial_reasons);
                }
                TaskTokenUsageScope::Unavailable { reason } => {
                    unavailable_count = unavailable_count.saturating_add(1);
                    extend_task_token_usage_reasons(&mut reasons, &[*reason]);
                }
            }
        }
        has_reported_usage |= has_reported_usage_floor;
        reasons.sort();
        let usage = if !has_reported_usage {
            TaskTokenUsageScope::Unavailable {
                reason: reasons
                    .first()
                    .copied()
                    .unwrap_or(TaskTokenUsageUnavailableReason::NoAssistantTurnCompleted),
            }
        } else if partial_seen || unavailable_count > 0 {
            TaskTokenUsageScope::Partial {
                usage: Box::new(TaskTokenUsageAmount::from_token_usage(reported_usage)),
                unavailable_count,
                reasons,
            }
        } else {
            TaskTokenUsageScope::Known {
                usage: Box::new(TaskTokenUsageAmount::from_token_usage(reported_usage)),
            }
        };
        (usage, self.latest_model.clone())
    }

    fn observe_chat_event(
        &mut self,
        event: &mut ChatEvent,
        source_seq: u64,
        active_stream_text: &str,
    ) -> bool {
        let previous = self.stats.clone();
        match event {
            ChatEvent::MessageAdded(message) => {
                if matches!(message.sender, MessageSender::Assistant { .. }) {
                    self.observe_model(message.model_info.as_ref());
                    self.update_last_output(&message.content, source_seq);
                    self.stamp_message_turn_token_usage(message, source_seq);
                }
            }
            ChatEvent::MessageMetadataUpdated(update) => {
                self.observe_model(update.model_info.as_ref());
                self.stamp_metadata_turn_token_usage(update, source_seq);
            }
            ChatEvent::StreamDelta(delta) => {
                if !delta.text.trim().is_empty() {
                    self.update_last_output(active_stream_text, source_seq);
                }
            }
            ChatEvent::StreamReasoningDelta(delta) => {
                self.active_reasoning.push_str(&delta.text);
                let active_reasoning = self.active_reasoning.clone();
                self.update_last_output(&active_reasoning, source_seq);
            }
            ChatEvent::StreamEnd(data) => {
                self.observe_model(data.message.model_info.as_ref());
                self.update_last_output(&data.message.content, source_seq);
                self.stamp_message_turn_token_usage(&mut data.message, source_seq);
                self.active_reasoning.clear();
            }
            ChatEvent::ToolRequest(request) => {
                if self.seen_tool_calls.insert(request.tool_call_id.clone()) {
                    self.stats.tool_calls = self.stats.tool_calls.saturating_add(1);
                    self.stats.source_through_seq = Some(source_seq);
                }
            }
            ChatEvent::TypingStatusChanged(_)
                if self.token_usage_tracking_mode == TokenUsageTrackingMode::Messages =>
            {
                self.commit_provisional_request_usage();
            }
            ChatEvent::TypingStatusChanged(_)
            | ChatEvent::ToolProgress(_)
            | ChatEvent::ToolExecutionCompleted(_)
            | ChatEvent::TaskUpdate(_)
            | ChatEvent::OperationCancelled(_)
            | ChatEvent::RetryAttempt(_)
            | ChatEvent::Orchestration(_) => {}
            ChatEvent::ContextCompaction(compaction)
                if compaction.status == ContextCompactionTimelineStatus::Completed =>
            {
                self.clear_current_context_usage(source_seq);
            }
            ChatEvent::ContextCompaction(_) => {}
            ChatEvent::StreamStart(data) => {
                if let Some(model) = data.model.as_ref().filter(|model| !model.trim().is_empty()) {
                    self.latest_model = Some(model.clone());
                }
                self.active_reasoning.clear();
            }
        }
        self.stats != previous
    }

    fn observe_model_request_token_usage(
        &mut self,
        usage: ModelRequestTokenUsage,
        source_seq: u64,
    ) -> bool {
        if self.token_usage_tracking_mode != TokenUsageTrackingMode::ModelRequests {
            return false;
        }
        let previous = self.stats.clone();
        self.token_usage_by_source.insert(
            TokenUsageSource::ModelRequest(usage.request_id),
            TaskTokenUsageScope::Known {
                usage: Box::new(TaskTokenUsageAmount::from_token_usage(&usage.request)),
            },
        );
        self.stats.token_usage = usage.cumulative;
        if let Some(current) = usage.current_context_usage {
            self.stats.estimated_context_breakdown = usage
                .estimated_context_breakdown
                .filter(|estimate| context_estimate_matches(estimate, &current));
            self.stats.current_context_usage = Some(current);
        }
        self.stats.source_through_seq = Some(source_seq);
        self.stats != previous
    }

    fn clear_current_context_usage(&mut self, source_seq: u64) -> bool {
        let cleared_usage = match self.token_usage_tracking_mode {
            TokenUsageTrackingMode::ModelRequests => Some(protocol::CurrentContextUsage::Unknown),
            TokenUsageTrackingMode::Messages => None,
        };
        if self.stats.current_context_usage == cleared_usage
            && self.stats.estimated_context_breakdown.is_none()
        {
            return false;
        }
        self.stats.current_context_usage = cleared_usage;
        self.stats.estimated_context_breakdown = None;
        self.stats.source_through_seq = Some(source_seq);
        true
    }

    fn observe_total_only_token_usage(&mut self, total_tokens: u64, source_seq: u64) -> bool {
        if self.token_usage_tracking_mode != TokenUsageTrackingMode::Messages {
            return false;
        }
        let previous = self.stats.clone();
        self.stats.token_usage_total_only = Some(
            self.stats
                .token_usage_total_only
                .unwrap_or_default()
                .max(total_tokens),
        );
        self.stats.source_through_seq = Some(source_seq);
        self.stats != previous
    }

    fn observe_model(&mut self, model_info: Option<&protocol::ModelInfo>) {
        let Some(model) = model_info
            .map(|info| info.model.trim())
            .filter(|model| !model.is_empty())
        else {
            return;
        };
        self.latest_model = Some(model.to_owned());
    }

    fn update_last_output(&mut self, text: &str, source_seq: u64) {
        let Some(line) = last_non_empty_logical_line(text) else {
            return;
        };
        if self.stats.last_output_line.as_ref() != Some(&line) {
            self.stats.last_output_line = Some(line);
            self.stats.source_through_seq = Some(source_seq);
        }
    }

    fn stamp_message_turn_token_usage(&mut self, message: &mut ChatMessage, source_seq: u64) {
        if self.token_usage_tracking_mode == TokenUsageTrackingMode::ModelRequests
            && message.token_usage.is_none()
        {
            return;
        }
        let source = token_usage_source_for_message(message, source_seq);
        message.token_usage = Some(self.scoped_token_usage_for_source(
            source,
            message.token_usage.clone(),
            source_seq,
        ));
    }

    fn stamp_metadata_turn_token_usage(
        &mut self,
        update: &mut MessageMetadataUpdateData,
        source_seq: u64,
    ) {
        let Some(token_usage) = update.token_usage.clone() else {
            return;
        };
        update.token_usage = Some(self.scoped_token_usage_for_source(
            TokenUsageSource::Message(update.message_id.clone()),
            Some(token_usage),
            source_seq,
        ));
    }

    fn scoped_token_usage_for_source(
        &mut self,
        source: TokenUsageSource,
        token_usage: Option<MessageTokenUsage>,
        source_seq: u64,
    ) -> MessageTokenUsage {
        let mut token_usage = token_usage.unwrap_or_else(|| {
            MessageTokenUsage::unavailable(TokenUsageUnavailableReason::BackendDidNotReport)
        });
        if self.token_usage_tracking_mode == TokenUsageTrackingMode::ModelRequests {
            return token_usage;
        }
        let Some(turn_usage) = token_usage.turn.known_usage().cloned() else {
            if let Some(request_usage) = token_usage.request.known_usage().cloned() {
                if !self.authoritative_turn_sources.contains(&source) {
                    if self
                        .authoritative_token_usage_by_source
                        .contains_key(&source)
                    {
                        self.authoritative_token_usage_by_source
                            .insert(source.clone(), request_usage.clone());
                    } else {
                        self.provisional_request_usage_by_source
                            .insert(source.clone(), request_usage.clone());
                    }
                }
                self.token_usage_by_source.insert(
                    source,
                    TaskTokenUsageScope::Known {
                        usage: Box::new(TaskTokenUsageAmount::from_token_usage(&request_usage)),
                    },
                );
                self.refresh_token_usage();
                self.stats.source_through_seq = Some(source_seq);
            } else {
                let reason = match token_usage.turn {
                    TokenUsageScope::Known { .. } => {
                        TaskTokenUsageUnavailableReason::AgentUnavailable
                    }
                    TokenUsageScope::Unavailable { reason } => {
                        task_token_usage_reason_from_message_reason(reason)
                    }
                };
                self.token_usage_by_source
                    .insert(source, TaskTokenUsageScope::Unavailable { reason });
            }
            return token_usage;
        };

        let existing_authoritative_turn = self.authoritative_turn_sources.contains(&source);
        if !existing_authoritative_turn {
            self.clear_provisional_request_usage();
        }
        self.token_usage_by_source.insert(
            source.clone(),
            TaskTokenUsageScope::Known {
                usage: Box::new(TaskTokenUsageAmount::from_token_usage(&turn_usage)),
            },
        );
        if let Some(cumulative) = token_usage.cumulative.known_usage().cloned() {
            self.authoritative_token_usage_baseline = cumulative;
            self.authoritative_token_usage_by_source.clear();
            self.token_usage_by_source
                .retain(|_, usage| matches!(usage, TaskTokenUsageScope::Known { .. }));
        } else {
            if !existing_authoritative_turn
                || self
                    .authoritative_token_usage_by_source
                    .contains_key(&source)
            {
                self.authoritative_token_usage_by_source
                    .insert(source.clone(), turn_usage);
            }
            self.refresh_token_usage();
            if !matches!(
                token_usage.cumulative,
                TokenUsageScope::Unavailable {
                    reason: TokenUsageUnavailableReason::ProviderScopeAmbiguous
                }
            ) {
                token_usage.cumulative = match synthesized_cumulative_unavailable_reason(
                    self.token_usage_by_source.values(),
                ) {
                    Some(reason) => TokenUsageScope::Unavailable { reason },
                    None => TokenUsageScope::Known {
                        usage: Box::new(self.stats.token_usage.clone()),
                    },
                };
            }
        }
        self.authoritative_turn_sources.insert(source);
        self.refresh_token_usage();
        self.stats.source_through_seq = Some(source_seq);
        token_usage
    }

    fn commit_provisional_request_usage(&mut self) {
        if self.token_usage_tracking_mode != TokenUsageTrackingMode::Messages
            || self.provisional_request_usage_by_source.is_empty()
        {
            return;
        }
        // A typing boundary ends the Messages-mode generation. If no turn
        // reconciled it, fold its request sources into the baseline and keep
        // one classification marker rather than retaining every request key.
        let promoted = total_token_usage(self.provisional_request_usage_by_source.values());
        self.authoritative_token_usage_baseline =
            total_token_usage([&self.authoritative_token_usage_baseline, &promoted].into_iter());
        for (source, _) in self.provisional_request_usage_by_source.drain() {
            self.token_usage_by_source.remove(&source);
        }
        self.token_usage_by_source.insert(
            TokenUsageSource::PromotedRequests,
            TaskTokenUsageScope::Known {
                usage: Box::new(TaskTokenUsageAmount::from_token_usage(&promoted)),
            },
        );
        self.refresh_token_usage();
    }

    fn clear_provisional_request_usage(&mut self) {
        for (source, _) in self.provisional_request_usage_by_source.drain() {
            self.token_usage_by_source.remove(&source);
        }
    }

    fn refresh_token_usage(&mut self) {
        self.stats.token_usage = total_token_usage(
            std::iter::once(&self.authoritative_token_usage_baseline)
                .chain(self.authoritative_token_usage_by_source.values())
                .chain(self.provisional_request_usage_by_source.values()),
        );
    }
}

fn total_token_usage<'a>(entries: impl Iterator<Item = &'a TokenUsage>) -> TokenUsage {
    let mut total = TokenUsage::default();
    for usage in entries {
        total.input_tokens = total.input_tokens.saturating_add(usage.input_tokens);
        total.output_tokens = total.output_tokens.saturating_add(usage.output_tokens);
        total.total_tokens = total.total_tokens.saturating_add(usage.total_tokens);
        add_optional_tokens(&mut total.cached_prompt_tokens, usage.cached_prompt_tokens);
        add_optional_tokens(
            &mut total.cache_creation_input_tokens,
            usage.cache_creation_input_tokens,
        );
        add_optional_tokens(&mut total.reasoning_tokens, usage.reasoning_tokens);
    }
    total
}

fn extend_task_token_usage_reasons(
    reasons: &mut Vec<TaskTokenUsageUnavailableReason>,
    additions: &[TaskTokenUsageUnavailableReason],
) {
    for reason in additions {
        if !reasons.contains(reason) {
            reasons.push(*reason);
        }
    }
}

fn task_token_usage_reason_from_message_reason(
    reason: TokenUsageUnavailableReason,
) -> TaskTokenUsageUnavailableReason {
    match reason {
        TokenUsageUnavailableReason::BackendDidNotReport => {
            TaskTokenUsageUnavailableReason::BackendDidNotReport
        }
        TokenUsageUnavailableReason::ProviderScopeAmbiguous => {
            TaskTokenUsageUnavailableReason::ProviderScopeAmbiguous
        }
    }
}

fn synthesized_cumulative_unavailable_reason<'a>(
    usages: impl Iterator<Item = &'a TaskTokenUsageScope>,
) -> Option<TokenUsageUnavailableReason> {
    let mut reason = None;
    for usage in usages {
        let Some(candidate) = (match usage {
            TaskTokenUsageScope::Known { .. } => None,
            TaskTokenUsageScope::Partial {
                reasons: task_reasons,
                ..
            } => Some(token_usage_reason_from_task_reasons(task_reasons)),
            TaskTokenUsageScope::Unavailable {
                reason: task_reason,
            } => Some(token_usage_reason_from_task_reason(*task_reason)),
        }) else {
            continue;
        };
        if candidate == TokenUsageUnavailableReason::ProviderScopeAmbiguous {
            return Some(candidate);
        }
        reason = reason.or(Some(candidate));
    }
    reason
}

fn token_usage_reason_from_task_reasons(
    reasons: &[TaskTokenUsageUnavailableReason],
) -> TokenUsageUnavailableReason {
    if reasons.contains(&TaskTokenUsageUnavailableReason::ProviderScopeAmbiguous) {
        TokenUsageUnavailableReason::ProviderScopeAmbiguous
    } else {
        TokenUsageUnavailableReason::BackendDidNotReport
    }
}

fn token_usage_reason_from_task_reason(
    reason: TaskTokenUsageUnavailableReason,
) -> TokenUsageUnavailableReason {
    match reason {
        TaskTokenUsageUnavailableReason::ProviderScopeAmbiguous => {
            TokenUsageUnavailableReason::ProviderScopeAmbiguous
        }
        TaskTokenUsageUnavailableReason::NoAssistantTurnCompleted
        | TaskTokenUsageUnavailableReason::BackendDidNotReport
        | TaskTokenUsageUnavailableReason::AgentUnavailable => {
            TokenUsageUnavailableReason::BackendDidNotReport
        }
    }
}

fn token_usage_source_for_message(message: &ChatMessage, source_seq: u64) -> TokenUsageSource {
    message
        .message_id
        .clone()
        .map(TokenUsageSource::Message)
        .unwrap_or(TokenUsageSource::EventSeq(source_seq))
}

fn last_non_empty_logical_line(text: &str) -> Option<String> {
    text.lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_owned)
}

fn known_turn_usage(token_usage: &Option<MessageTokenUsage>) -> Option<&TokenUsage> {
    token_usage.as_ref()?.turn.known_usage()
}

fn add_optional_tokens(total: &mut Option<u64>, value: Option<u64>) {
    if let Some(value) = value {
        *total = Some(total.unwrap_or(0).saturating_add(value));
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InterruptOutcome {
    Interrupted,
    Rejected,
    NotRunning,
}

impl AgentHandle {
    pub async fn send_input(&self, input: AgentInput) -> bool {
        if self.closing.load(Ordering::SeqCst) {
            return false;
        }
        self.tx.send(AgentCommand::SendInput(input)).is_ok()
    }

    /// Whether this handle has been closed.
    ///
    /// Exists so a caller that just saw [`AgentHandle::send_input`] fail can
    /// tell the user *why* rather than defaulting to "agent not running" — a
    /// closing agent is still running, and saying otherwise sends people
    /// looking for a crash that never happened.
    ///
    /// The flag is monotonic: close admission sets it before teardown and
    /// never clears it. Reading it after a failed send is therefore not a race
    /// — if it reads true now it was already true when the send was refused.
    pub(crate) fn is_closing(&self) -> bool {
        self.closing.load(Ordering::SeqCst)
    }

    pub(crate) fn begin_closing(&self) {
        self.closing.store(true, Ordering::SeqCst);
        self.accepting_input.store(false, Ordering::SeqCst);
    }

    /// Delivers an agent-control follow-up and waits for the actor's own
    /// acceptance.
    ///
    /// [`AgentHandle::send_input`] is fire-and-forget: it reports only that the
    /// mailbox took the command, which is why a parked terminal actor can
    /// accept one and answer with a typed transcript rejection. That behavior
    /// is load-bearing for client/router input and is unchanged.
    ///
    /// This method is the checked counterpart. It returns `Ok(())` only once
    /// the actor has accepted or queued the message **and** published an active
    /// turn through its own status handle, so a caller that immediately waits
    /// on the agent cannot observe a stale idle snapshot. Every state in which
    /// the actor drops the message instead — relay, active or blocked
    /// compaction, closing, parked terminal, dead mailbox — returns `Err`
    /// without touching the target's status and without appending a second
    /// transcript error for a message that was never seen.
    pub(crate) async fn deliver_message(&self, payload: SendMessagePayload) -> Result<(), String> {
        if self.closing.load(Ordering::SeqCst) {
            return Err(DELIVERY_REJECTED_CLOSING.to_owned());
        }
        let (reply_tx, reply_rx) = oneshot::channel();
        if self
            .tx
            .send(AgentCommand::DeliverMessage {
                payload,
                reply: reply_tx,
            })
            .is_err()
        {
            return Err(DELIVERY_REJECTED_MAILBOX_CLOSED.to_owned());
        }
        // A dropped acknowledgement means the actor ended without resolving the
        // delivery. Fail closed: the caller must never read that as delivered.
        reply_rx
            .await
            .unwrap_or_else(|_| Err(DELIVERY_NOT_ACKNOWLEDGED.to_owned()))
    }

    pub fn begin_compact(
        &self,
        summary_prompt: String,
        max_summary_bytes: usize,
    ) -> CompactionStart {
        if !self.accepting_input.load(Ordering::SeqCst) {
            return CompactionStart::Rejected("agent is not accepting input".to_owned());
        }
        let (reply_tx, reply_rx) = oneshot::channel();
        if self
            .tx
            .send(AgentCommand::Compact {
                summary_prompt,
                max_summary_bytes,
                reply: reply_tx,
            })
            .is_err()
        {
            return CompactionStart::Closed;
        }
        CompactionStart::Started(reply_rx)
    }

    pub async fn begin_compact_if_inactive(
        &self,
        expected_activity_counter: u64,
        expected_supervisor_settings_epoch: u64,
        supervisor_settings_rx: watch::Receiver<crate::host::SupervisorSettingsSignal>,
        summary_prompt: String,
        max_summary_bytes: usize,
    ) -> CompactionStart {
        if !self.accepting_input.load(Ordering::SeqCst) {
            return CompactionStart::Rejected("agent is not accepting input".to_owned());
        }
        let (accepted_tx, accepted_rx) = oneshot::channel();
        let (reply_tx, reply_rx) = oneshot::channel();
        if self
            .tx
            .send(AgentCommand::CompactIfInactive {
                expected_activity_counter,
                expected_supervisor_settings_epoch,
                supervisor_settings_rx,
                summary_prompt,
                max_summary_bytes,
                accepted: accepted_tx,
                reply: reply_tx,
            })
            .is_err()
        {
            return CompactionStart::Closed;
        }
        match accepted_rx.await {
            Ok(Ok(())) => CompactionStart::Started(reply_rx),
            Ok(Err(error)) => CompactionStart::Rejected(error),
            Err(_) => CompactionStart::Closed,
        }
    }

    pub async fn release_compaction(&self) -> bool {
        let (reply_tx, reply_rx) = oneshot::channel();
        if self
            .tx
            .send(AgentCommand::ReleaseCompaction { reply: reply_tx })
            .is_err()
        {
            return false;
        }
        reply_rx.await.is_ok()
    }

    pub(crate) async fn compaction_capability(
        &self,
    ) -> Option<crate::backend::BackendCompactionCapability> {
        let (reply, received) = oneshot::channel();
        self.tx
            .send(AgentCommand::ReadCompactionCapability { reply })
            .ok()?;
        received.await.ok()
    }

    pub(crate) async fn requested_compaction_route(
        &self,
        trigger: CompactionTrigger,
    ) -> Result<protocol::RequestedCompactionRoute, String> {
        let (reply, received) = oneshot::channel();
        self.tx
            .send(AgentCommand::ReadRequestedCompactionRoute { trigger, reply })
            .map_err(|_| "agent stopped before compaction route was read".to_owned())?;
        received
            .await
            .map_err(|_| "agent stopped before compaction route was read".to_owned())?
    }

    pub(crate) async fn request_context_compaction(
        &self,
        trigger: CompactionTrigger,
        focus: Option<String>,
        barrier_timeout: Duration,
    ) -> Result<CompactionOperationId, String> {
        if !self.accepting_input.load(Ordering::SeqCst) {
            return Err("agent is not accepting input".to_owned());
        }
        let (reply, received) = oneshot::channel();
        self.tx
            .send(AgentCommand::RequestContextCompaction {
                trigger,
                focus,
                barrier_timeout,
                inactivity_gate: None,
                reply,
            })
            .map_err(|_| "agent stopped before compaction was admitted".to_owned())?;
        received
            .await
            .map_err(|_| "agent stopped before compaction was admitted".to_owned())?
    }

    pub(crate) async fn request_context_compaction_if_inactive(
        &self,
        expected_activity_counter: u64,
        expected_supervisor_settings_epoch: u64,
        supervisor_settings_rx: watch::Receiver<crate::host::SupervisorSettingsSignal>,
        barrier_timeout: Duration,
    ) -> Result<CompactionOperationId, String> {
        if !self.accepting_input.load(Ordering::SeqCst) {
            return Err("agent is not accepting input".to_owned());
        }
        let (reply, received) = oneshot::channel();
        self.tx
            .send(AgentCommand::RequestContextCompaction {
                trigger: CompactionTrigger::SupervisorRequested,
                focus: None,
                barrier_timeout,
                inactivity_gate: Some((
                    expected_activity_counter,
                    expected_supervisor_settings_epoch,
                    supervisor_settings_rx,
                )),
                reply,
            })
            .map_err(|_| "agent stopped before compaction was admitted".to_owned())?;
        received
            .await
            .map_err(|_| "agent stopped before compaction was admitted".to_owned())?
    }

    pub async fn set_name(&self, name: String) -> Option<bool> {
        self.set_name_with_persistence(name, InitialAgentAliasPersistence::User)
            .await
    }

    async fn set_name_with_persistence(
        &self,
        name: String,
        persistence: InitialAgentAliasPersistence,
    ) -> Option<bool> {
        let (reply_tx, reply_rx) = oneshot::channel();
        if self
            .tx
            .send(AgentCommand::SetName {
                name,
                persistence,
                reply: reply_tx,
            })
            .is_err()
        {
            return None;
        }
        reply_rx.await.ok()
    }

    pub async fn apply_generated_name(&self, result: Result<String, String>) -> Option<bool> {
        let (reply_tx, reply_rx) = oneshot::channel();
        if self
            .tx
            .send(AgentCommand::ApplyGeneratedName {
                result,
                reply: reply_tx,
            })
            .is_err()
        {
            return None;
        }
        reply_rx.await.ok()
    }

    pub fn snapshot(&self) -> AgentStartPayload {
        self.start.borrow().clone()
    }

    pub async fn read_output(&self, after_seq: Option<u64>, limit: usize) -> Option<Vec<Envelope>> {
        let (reply_tx, reply_rx) = oneshot::channel();
        if self
            .tx
            .send(AgentCommand::ReadOutput {
                after_seq,
                limit,
                reply: reply_tx,
            })
            .is_err()
        {
            return None;
        }
        reply_rx.await.ok()
    }

    pub async fn read_latest_output(&self) -> Option<Result<AgentControlOutput, String>> {
        let (reply_tx, reply_rx) = oneshot::channel();
        if self
            .tx
            .send(AgentCommand::ReadLatestOutput { reply: reply_tx })
            .is_err()
        {
            return None;
        }
        reply_rx.await.ok()
    }

    pub async fn fetch_session_history(
        &self,
        before_seq: Option<u64>,
        limit: usize,
    ) -> Option<SessionHistoryWindow> {
        let (reply_tx, reply_rx) = oneshot::channel();
        if self
            .tx
            .send(AgentCommand::FetchSessionHistory {
                before_seq,
                limit,
                reply: reply_tx,
            })
            .is_err()
        {
            return None;
        }
        reply_rx.await.ok()
    }

    pub async fn read_activity_history(
        &self,
        after_seq: Option<u64>,
        max_events: usize,
        max_bytes: usize,
    ) -> Option<AgentActivityHistorySnapshot> {
        let (reply_tx, reply_rx) = oneshot::channel();
        if self
            .tx
            .send(AgentCommand::ReadActivityHistory {
                after_seq,
                max_events,
                max_bytes,
                reply: reply_tx,
            })
            .is_err()
        {
            return None;
        }
        reply_rx.await.ok()
    }

    pub async fn read_usage_snapshot(&self) -> Option<AgentUsageSnapshot> {
        let (reply_tx, reply_rx) = oneshot::channel();
        if self
            .tx
            .send(AgentCommand::ReadUsageSnapshot { reply: reply_tx })
            .is_err()
        {
            return None;
        }
        reply_rx.await.ok()
    }

    pub async fn interrupt(&self) -> InterruptOutcome {
        let (reply_tx, reply_rx) = oneshot::channel();
        if self
            .tx
            .send(AgentCommand::Interrupt { reply: reply_tx })
            .is_err()
        {
            return InterruptOutcome::NotRunning;
        }
        reply_rx.await.unwrap_or(InterruptOutcome::NotRunning)
    }

    pub async fn close(&self) -> bool {
        self.begin_closing();
        let (reply_tx, reply_rx) = oneshot::channel();
        if self
            .tx
            .send(AgentCommand::Close { reply: reply_tx })
            .is_err()
        {
            return false;
        }
        reply_rx.await.is_ok()
    }

    #[cfg(feature = "test-support")]
    pub async fn force_backend_shutdown_for_conformance(&self) -> bool {
        let (reply_tx, reply_rx) = oneshot::channel();
        if self
            .tx
            .send(AgentCommand::ForceBackendShutdownForConformance { reply: reply_tx })
            .is_err()
        {
            return false;
        }
        reply_rx.await.unwrap_or(false)
    }

    /// Test support: the live mock backend's control handle, or `None` when
    /// this agent has no running mock backend.
    #[cfg(feature = "test-support")]
    pub async fn mock_control(&self) -> Option<crate::backend::mock::MockControl> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(AgentCommand::ReadMockControl { reply: reply_tx })
            .ok()?;
        reply_rx.await.ok().flatten()
    }

    pub fn begin_attach(&self, stream: Stream) -> Option<oneshot::Receiver<bool>> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(AgentCommand::Attach {
                stream,
                reply: reply_tx,
            })
            .ok()?;
        Some(reply_rx)
    }

    pub async fn attach(&self, stream: Stream) -> bool {
        let Some(reply_rx) = self.begin_attach(stream) else {
            return false;
        };
        reply_rx.await.unwrap_or(false)
    }
}

#[cfg(feature = "test-support")]
type StartupCompletionTestGates =
    std::sync::Mutex<HashMap<String, Arc<crate::host::SpawnOperationTestGateInner>>>;

#[cfg(feature = "test-support")]
type StartupBackendReadyTestGates =
    std::sync::Mutex<HashMap<String, Arc<crate::host::SpawnOperationTestGateInner>>>;

#[cfg(feature = "test-support")]
fn startup_completion_test_gates() -> &'static StartupCompletionTestGates {
    static GATES: std::sync::OnceLock<StartupCompletionTestGates> = std::sync::OnceLock::new();
    GATES.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

#[cfg(feature = "test-support")]
fn startup_backend_ready_test_gates() -> &'static StartupBackendReadyTestGates {
    static GATES: std::sync::OnceLock<StartupBackendReadyTestGates> = std::sync::OnceLock::new();
    GATES.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

#[cfg(feature = "test-support")]
pub(crate) fn install_startup_completion_test_gate(
    agent_name: String,
    gate: Arc<crate::host::SpawnOperationTestGateInner>,
) {
    let replaced = startup_completion_test_gates()
        .lock()
        .expect("startup completion test gate mutex poisoned")
        .insert(agent_name, gate);
    assert!(
        replaced.is_none(),
        "startup completion test gate already installed"
    );
}

#[cfg(feature = "test-support")]
pub(crate) fn install_startup_backend_ready_test_gate(
    agent_name: String,
    gate: Arc<crate::host::SpawnOperationTestGateInner>,
) {
    let replaced = startup_backend_ready_test_gates()
        .lock()
        .expect("startup backend-ready test gate mutex poisoned")
        .insert(agent_name, gate);
    assert!(
        replaced.is_none(),
        "startup backend-ready test gate already installed"
    );
}

#[cfg(feature = "test-support")]
async fn wait_for_startup_completion_test_gate(agent_name: &str) {
    let gate = startup_completion_test_gates()
        .lock()
        .expect("startup completion test gate mutex poisoned")
        .get(agent_name)
        .cloned();
    if let Some(gate) = gate {
        eprintln!("TYDE STARTUP PRE-READY GATE ENTER name={agent_name}");
        crate::host::wait_for_spawn_operation_test_gate_inner(&gate).await;
        eprintln!("TYDE STARTUP PRE-READY GATE RELEASE name={agent_name}");
        startup_completion_test_gates()
            .lock()
            .expect("startup completion test gate mutex poisoned")
            .remove(agent_name);
    }
}

#[cfg(feature = "test-support")]
async fn wait_for_startup_backend_ready_test_gate(agent_name: &str) {
    let gate = startup_backend_ready_test_gates()
        .lock()
        .expect("startup backend-ready test gate mutex poisoned")
        .get(agent_name)
        .cloned();
    if let Some(gate) = gate {
        eprintln!("TYDE STARTUP READY GATE ENTER name={agent_name}");
        crate::host::wait_for_spawn_operation_test_gate_inner(&gate).await;
        eprintln!("TYDE STARTUP READY GATE RELEASE name={agent_name}");
        startup_backend_ready_test_gates()
            .lock()
            .expect("startup backend-ready test gate mutex poisoned")
            .remove(agent_name);
    }
}

#[cfg(feature = "test-support")]
fn notify_startup_name_stashed_test_gate(agent_name: &str) {
    let gate = startup_completion_test_gates()
        .lock()
        .expect("startup completion test gate mutex poisoned")
        .get(agent_name)
        .cloned();
    if let Some(gate) = gate {
        crate::host::notify_spawn_operation_test_gate_inner(&gate);
    }
}

#[cfg(feature = "test-support")]
type ResumeQueueDispatchTestGates =
    std::sync::Mutex<HashMap<String, Arc<crate::host::SpawnOperationTestGateInner>>>;

#[cfg(feature = "test-support")]
fn resume_queue_dispatch_test_gates() -> &'static ResumeQueueDispatchTestGates {
    static GATES: std::sync::OnceLock<ResumeQueueDispatchTestGates> = std::sync::OnceLock::new();
    GATES.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

#[cfg(feature = "test-support")]
pub(crate) fn install_resume_queue_dispatch_test_gate(
    agent_name: String,
    gate: Arc<crate::host::SpawnOperationTestGateInner>,
) {
    let replaced = resume_queue_dispatch_test_gates()
        .lock()
        .expect("resume queue dispatch test gate mutex poisoned")
        .insert(agent_name, gate);
    assert!(
        replaced.is_none(),
        "resume queue dispatch test gate already installed"
    );
}

#[cfg(feature = "test-support")]
async fn wait_for_resume_queue_dispatch_test_gate(agent_name: &str) {
    let gate = resume_queue_dispatch_test_gates()
        .lock()
        .expect("resume queue dispatch test gate mutex poisoned")
        .get(agent_name)
        .cloned();
    if let Some(gate) = gate {
        crate::host::wait_for_spawn_operation_test_gate_inner(&gate).await;
        resume_queue_dispatch_test_gates()
            .lock()
            .expect("resume queue dispatch test gate mutex poisoned")
            .remove(agent_name);
    }
}

#[cfg(not(feature = "test-support"))]
async fn wait_for_resume_queue_dispatch_test_gate(_agent_name: &str) {}

#[cfg(feature = "test-support")]
async fn hold_resume_queue_dispatch_boundary(
    agent_name: &str,
    backend: &mut Option<BackendHandle>,
    actor_tx: &mpsc::UnboundedSender<AgentCommand>,
    rx: &mut mpsc::UnboundedReceiver<AgentCommand>,
) -> bool {
    let mut gate = Box::pin(wait_for_resume_queue_dispatch_test_gate(agent_name));
    let mut deferred = Vec::new();
    let mut forced_closed = false;
    let mut rx_open = true;
    loop {
        tokio::select! {
            biased;
            command = rx.recv(), if rx_open => {
                match command {
                    Some(AgentCommand::ForceBackendShutdownForConformance { reply }) => {
                        let closed = if let Some(live_backend) = backend.take() {
                            live_backend.shutdown().await;
                            forced_closed = true;
                            true
                        } else {
                            false
                        };
                        let _ = reply.send(closed);
                    }
                    Some(command) => deferred.push(command),
                    None => rx_open = false,
                }
            }
            () = &mut gate => break,
        }
    }
    for command in deferred {
        let _ = actor_tx.send(command);
    }
    forced_closed
}

#[cfg(not(feature = "test-support"))]
async fn hold_resume_queue_dispatch_boundary(
    _agent_name: &str,
    _backend: &mut Option<BackendHandle>,
    _actor_tx: &mpsc::UnboundedSender<AgentCommand>,
    _rx: &mut mpsc::UnboundedReceiver<AgentCommand>,
) -> bool {
    false
}

enum ActorLifecycle {
    Running,
    Closing,
}

pub(crate) struct GenerateAgentNameRequest {
    pub backend_kind: BackendKind,
    pub prompt: String,
    pub session_settings: Option<SessionSettingsValues>,
    pub use_mock_backend: bool,
    pub capacity_tx: HostCapacityTx,
}

pub(crate) struct GenerateAgentActivitySummaryRequest {
    pub summary_agent_id: AgentId,
    pub backend_kind: BackendKind,
    pub workspace_roots: Vec<String>,
    pub rendered_history: String,
    pub previous_summary: Option<String>,
    pub source_from_seq: Option<u64>,
    pub source_through_seq: Option<u64>,
    pub session_settings: Option<SessionSettingsValues>,
    pub use_mock_backend: bool,
    pub capacity_tx: HostCapacityTx,
}

struct PrepareContextFallbackRequest {
    backend_kind: BackendKind,
    workspace_roots: Vec<String>,
    logical_session_id: SessionId,
    transcript_store: TranscriptStore,
    transcript_high_water: u64,
    requested_focus: Option<String>,
    spawn_config: BackendSpawnConfig,
    use_mock_backend: bool,
    capacity_tx: HostCapacityTx,
    antigravity_conversations_dir: PathBuf,
}

/// Starts one Tyde naming-helper turn. A backend may satisfy that turn with
/// one or more provider calls; Hermes, for example, continues internally after
/// `finish_reason=length`. Tyde does not add a naming retry on top of that
/// backend-owned 1..N call sequence.
pub(crate) async fn generate_agent_name(
    request: GenerateAgentNameRequest,
) -> Result<String, String> {
    let prompt = request.prompt.trim();
    if prompt.is_empty() {
        return Ok(IMAGE_ONLY_AGENT_NAME.to_string());
    }

    if request.use_mock_backend {
        if prompt.contains("__mock_async_generated_name__") {
            return Ok("Generated Async Name".to_owned());
        }
        return generate_mock_name(prompt);
    }

    let name_prompt = build_name_generation_prompt(prompt);
    let logged_name_prompt = name_prompt.clone();
    let spawn_config = agent_name_generation_spawn_config(request.session_settings.clone());
    let isolated_workspace = tempfile::tempdir()
        .map_err(|err| format!("failed to create isolated agent naming workspace: {err}"))?;
    let workspace_roots = vec![isolated_workspace.path().to_string_lossy().into_owned()];
    let initial_input = SendMessagePayload {
        message: name_prompt,
        images: None,
        origin: None,
        tool_response: None,
    };
    let name_agent_id = AgentId(Uuid::new_v4().to_string());
    let (host_sub_agent_spawn_tx, _host_sub_agent_spawn_rx) = mpsc::unbounded_channel();
    let (_backend, mut events, _session_id) = match spawn_backend(
        &name_agent_id,
        request.backend_kind,
        workspace_roots,
        spawn_config,
        initial_input,
        HostSubAgentEmitterContext {
            host_sub_agent_spawn_tx,
            capacity_tx: request.capacity_tx.clone(),
        },
        None,
    )
    .await
    {
        Ok(result) => result,
        Err(err) => {
            return Err(format!(
                "agent name generator failed to start for backend {:?}: {}",
                request.backend_kind, err
            ));
        }
    };

    let result = collect_agent_name_events(&mut events).await;
    if let Err(err) = &result {
        tracing::warn!(
            backend_kind = ?request.backend_kind,
            cost_hint = ?SpawnCostHint::Low,
            prompt = %prompt,
            name_prompt = %logged_name_prompt,
            error = %err,
            "agent name generator failed"
        );
    }
    result
}

pub(crate) fn agent_name_generation_spawn_config(
    session_settings: Option<SessionSettingsValues>,
) -> BackendSpawnConfig {
    BackendSpawnConfig {
        execution_mode: BackendExecutionMode::InferenceOnly,
        cost_hint: Some(SpawnCostHint::Low),
        custom_agent_id: None,
        startup_mcp_servers: Vec::new(),
        session_settings,
        provider_version: None,
        antigravity_conversations_dir: None,
        backend_config: Default::default(),
        acp_agent: None,
        resolved_spawn_config: customization::ResolvedSpawnConfig {
            tool_policy: ToolPolicy::AllowList { tools: Vec::new() },
            access_mode: BackendAccessMode::ReadOnly,
            ..Default::default()
        },
    }
}

async fn collect_agent_name_events(events: &mut EventStream) -> Result<String, String> {
    let mut streamed_text = String::new();
    // Some backends run session-setup commands before the naming turn, and
    // each command completion emits its own typing false (captured live on
    // the Tycode wire: SetRootAgent produces typing true → RootAgentChanged →
    // typing false before the prompt turn starts). Typing false only means
    // "turn completed without a response" once the turn itself has produced a
    // message or stream frame; earlier ones are setup noise. A backend that
    // never produces either is bounded by await_agent_name_generation's
    // timeout rather than misread here.
    let mut turn_started = false;
    while let Some(event) = events.recv().await {
        match event {
            ChatEvent::MessageAdded(message) if matches!(message.sender, MessageSender::Error) => {
                return Err(message.content);
            }
            ChatEvent::MessageAdded(_) | ChatEvent::StreamStart(_) => {
                turn_started = true;
            }
            ChatEvent::StreamDelta(delta) => {
                turn_started = true;
                streamed_text.push_str(&delta.text);
            }
            ChatEvent::StreamEnd(data) => {
                turn_started = true;
                let final_content = data.message.content;
                let candidate = if final_content.trim().is_empty() {
                    std::mem::take(&mut streamed_text)
                } else {
                    final_content
                };
                if candidate.trim().is_empty() {
                    continue;
                }
                return sanitize_generated_agent_name(&candidate);
            }
            ChatEvent::TypingStatusChanged(false) if turn_started => {
                return Err(
                    "agent name generator turn completed before producing a final response"
                        .to_string(),
                );
            }
            _ => {}
        }
    }

    Err("agent name generator ended before producing a final response".to_string())
}

pub(crate) async fn generate_agent_activity_summary(
    request: GenerateAgentActivitySummaryRequest,
) -> Result<AgentActivitySummary, String> {
    let rendered_history = request.rendered_history.trim();
    if rendered_history.is_empty() {
        return Err("activity summary input was empty".to_owned());
    }

    if request.use_mock_backend {
        return generate_mock_activity_summary(request).await;
    }

    let prompt =
        build_activity_summary_prompt(rendered_history, request.previous_summary.as_deref());
    let logged_prompt_len = prompt.len();
    let target_workspace_root_count = request.workspace_roots.len();
    let spawn_config = agent_name_generation_spawn_config(request.session_settings.clone());
    let initial_input = SendMessagePayload {
        message: prompt,
        images: None,
        origin: None,
        tool_response: None,
    };
    let (host_sub_agent_spawn_tx, _host_sub_agent_spawn_rx) = mpsc::unbounded_channel();
    let isolated_workspace = tempfile::tempdir()
        .map_err(|err| format!("failed to create isolated activity summary workspace: {err}"))?;
    let workspace_roots = vec![isolated_workspace.path().to_string_lossy().into_owned()];
    let (_backend, mut events, _session_id) = match spawn_backend(
        &request.summary_agent_id,
        request.backend_kind,
        workspace_roots,
        spawn_config,
        initial_input,
        HostSubAgentEmitterContext {
            host_sub_agent_spawn_tx,
            capacity_tx: request.capacity_tx.clone(),
        },
        None,
    )
    .await
    {
        Ok(result) => result,
        Err(err) => {
            return Err(format!(
                "agent activity summary generator failed to start for backend {:?}: {}",
                request.backend_kind, err
            ));
        }
    };

    collect_agent_activity_summary_events(
        &request,
        &mut events,
        logged_prompt_len,
        target_workspace_root_count,
    )
    .await
}

async fn prepare_context_fallback(
    request: PrepareContextFallbackRequest,
) -> Result<PreparedContextFallback, String> {
    wait_for_context_fallback_test_gate(&request.logical_session_id).await;
    let rendered_transcript = load_authoritative_compaction_transcript(
        &request.transcript_store,
        &request.logical_session_id,
        request.transcript_high_water,
    )
    .await?;
    let before_messages = rendered_transcript
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count() as u64;
    let summary = generate_fallback_compaction_summary(
        request.backend_kind,
        rendered_transcript,
        request.requested_focus.as_deref(),
        request.spawn_config.session_settings.clone(),
        request.use_mock_backend,
        request.capacity_tx,
        request.antigravity_conversations_dir.clone(),
    )
    .await?;

    let seed = crate::backend::BackendContextSeed {
        workspace_roots: request.workspace_roots,
        summary,
    };
    let binding = if request.use_mock_backend {
        crate::backend::prepare_mock_compacted_backend_binding(
            request.backend_kind,
            request.spawn_config,
            seed,
        )
        .await
    } else {
        crate::backend::prepare_compacted_backend_binding(
            request.backend_kind,
            request.spawn_config,
            seed,
        )
        .await
    }
    .map_err(|error| error.to_string())?;
    if binding.ready.backend_kind != request.backend_kind
        || binding.ready.provider_session_id != binding.provider_session_id
        || !binding.ready.bootstrap_terminal_seen
        || !binding.ready.provider_idle_seen
        || !binding.ready.replay_or_setup_drained
        || binding.ready.unsafe_activity_observed
    {
        binding.backend.shutdown().await;
        return Err(
            "prepared backend binding did not provide complete readiness evidence".to_owned(),
        );
    }
    Ok(PreparedContextFallback {
        binding,
        metrics: CompactionMetrics {
            before_messages: Some(before_messages),
            after_messages: Some(1),
            messages_summarized: Some(before_messages),
            precomputed: Some(true),
            ..CompactionMetrics::default()
        },
    })
}

async fn load_authoritative_compaction_transcript(
    store: &TranscriptStore,
    session_id: &SessionId,
    high_water: u64,
) -> Result<String, String> {
    if !store.actor_io_enabled() {
        return Err("canonical transcript storage is unavailable".to_owned());
    }
    let session_id = session_id.clone();
    let store = store.clone();
    tokio::task::spawn_blocking(move || {
        if !store.is_authoritative(&session_id) {
            return Err("canonical transcript is not authoritative".to_owned());
        }
        let mut rendered = String::new();
        for record in store.load(&session_id)? {
            if record.sequence > high_water
                || !matches!(
                    record.visibility,
                    crate::store::transcript::TranscriptVisibility::Visible
                        | crate::store::transcript::TranscriptVisibility::TimelineMarker
                )
            {
                continue;
            }
            let event = serde_json::to_string(&record.event)
                .map_err(|error| format!("failed to render canonical transcript: {error}"))?;
            rendered.push_str(&event);
            rendered.push('\n');
        }
        if rendered.trim().is_empty() {
            return Err("canonical transcript contains no visible context".to_owned());
        }
        Ok(rendered)
    })
    .await
    .map_err(|error| format!("canonical transcript read task failed: {error}"))?
}

async fn generate_fallback_compaction_summary(
    backend_kind: BackendKind,
    rendered_transcript: String,
    requested_focus: Option<&str>,
    session_settings: Option<SessionSettingsValues>,
    use_mock_backend: bool,
    capacity_tx: HostCapacityTx,
    antigravity_conversations_dir: PathBuf,
) -> Result<String, String> {
    if use_mock_backend {
        return Ok(format!(
            "Mock compacted context preserving {} canonical transcript bytes.",
            rendered_transcript.len()
        ));
    }
    let focus = requested_focus
        .map(str::trim)
        .filter(|focus| !focus.is_empty())
        .unwrap_or("Preserve the active task, decisions, constraints, and next steps.");
    let prompt = format!(
        "Produce a faithful compacted working-context handoff from the canonical transcript below. \
Return only the handoff, with no preamble. Preserve active tasks, decisions, constraints, exact \
identifiers, unresolved failures, and concrete next steps. Do not call tools. Requested focus: \
{focus}\n\nCanonical transcript:\n{rendered_transcript}"
    );
    let spawn_config = agent_name_generation_spawn_config(session_settings);
    let initial_input = SendMessagePayload {
        message: prompt,
        images: None,
        origin: None,
        tool_response: None,
    };
    let isolated_workspace = tempfile::tempdir().map_err(|error| {
        format!("failed to create isolated compaction summary workspace: {error}")
    })?;
    let workspace_roots = vec![isolated_workspace.path().to_string_lossy().into_owned()];
    let (host_sub_agent_spawn_tx, _host_sub_agent_spawn_rx) = mpsc::unbounded_channel();
    let summary_agent_id = AgentId(Uuid::new_v4().to_string());
    let (_backend, mut events, _session_id) = spawn_backend(
        &summary_agent_id,
        backend_kind,
        workspace_roots,
        spawn_config,
        initial_input,
        HostSubAgentEmitterContext {
            host_sub_agent_spawn_tx,
            capacity_tx,
        },
        Some(antigravity_conversations_dir),
    )
    .await
    .map_err(|error| format!("fallback summary generator failed to start: {error}"))?;
    let collect = async {
        let mut summary = String::new();
        while let Some(event) = events.recv().await {
            match event {
                ChatEvent::StreamDelta(delta) => {
                    push_summary_capped(&mut summary, &delta.text, MAX_COMPACTION_SUMMARY_BYTES);
                }
                ChatEvent::StreamEnd(end) if summary.trim().is_empty() => {
                    push_summary_capped(
                        &mut summary,
                        &end.message.content,
                        MAX_COMPACTION_SUMMARY_BYTES,
                    );
                }
                ChatEvent::MessageAdded(message)
                    if matches!(message.sender, MessageSender::Error) =>
                {
                    return Err(message.content);
                }
                ChatEvent::ToolRequest(request) => {
                    return Err(format!(
                        "fallback summary generator attempted tool {}",
                        tool_request_label(&request.tool_type)
                    ));
                }
                ChatEvent::TypingStatusChanged(false) if !summary.trim().is_empty() => break,
                _ => {}
            }
        }
        let summary = summary.trim().to_owned();
        if summary.is_empty() {
            Err("fallback summary generator produced no context".to_owned())
        } else {
            Ok(summary)
        }
    };
    tokio::time::timeout(Duration::from_secs(300), collect)
        .await
        .map_err(|_| "fallback summary generator timed out".to_owned())?
}

async fn collect_agent_activity_summary_events(
    request: &GenerateAgentActivitySummaryRequest,
    events: &mut EventStream,
    logged_prompt_len: usize,
    target_workspace_root_count: usize,
) -> Result<AgentActivitySummary, String> {
    let mut streamed_text = String::new();
    let mut stream_delta_count = 0usize;
    let mut chat_event_count = 0usize;
    let mut stream_end_without_usable_text_count = 0usize;
    let mut backend_error: Option<String> = None;
    let mut attempted_tools = Vec::new();
    while let Some(event) = events.recv().await {
        chat_event_count += 1;
        match event {
            ChatEvent::MessageAdded(message) if matches!(message.sender, MessageSender::Error) => {
                backend_error = Some(message.content);
            }
            ChatEvent::StreamDelta(delta) => {
                stream_delta_count += 1;
                streamed_text.push_str(&delta.text);
            }
            ChatEvent::StreamEnd(data) => {
                let final_content = data.message.content;
                if let Some(text) = sanitize_activity_summary_candidate_text([
                    final_content.as_str(),
                    streamed_text.as_str(),
                ]) {
                    return Ok(AgentActivitySummary {
                        text,
                        generated_at_ms: now_ms(),
                        source_from_seq: request.source_from_seq,
                        source_through_seq: request.source_through_seq,
                    });
                }
                stream_end_without_usable_text_count =
                    stream_end_without_usable_text_count.saturating_add(1);
                let attempted_tool_labels =
                    activity_summary_attempted_tool_labels(&attempted_tools);
                tracing::debug!(
                    summary_agent_id = %request.summary_agent_id,
                    backend_kind = ?request.backend_kind,
                    cost_hint = ?SpawnCostHint::Low,
                    prompt_len = logged_prompt_len,
                    target_workspace_root_count,
                    chat_event_count,
                    stream_delta_count,
                    stream_end_without_usable_text_count,
                    final_content_len = final_content.len(),
                    streamed_text_len = streamed_text.len(),
                    backend_error = ?backend_error.as_deref(),
                    attempted_tool_count = attempted_tools.len(),
                    attempted_tools = %attempted_tool_labels,
                    "agent activity summary generator stream segment ended without usable assistant text"
                );
            }
            ChatEvent::ToolRequest(requested_tool) => {
                let tool_name = tool_request_label(&requested_tool.tool_type).to_string();
                let tool_call_id = requested_tool.tool_call_id;
                tracing::warn!(
                    summary_agent_id = %request.summary_agent_id,
                    backend_kind = ?request.backend_kind,
                    tool_name = %tool_name,
                    tool_call_id = %tool_call_id,
                    "activity summary generator attempted a tool call; ignoring and continuing"
                );
                attempted_tools.push(ActivitySummaryToolAttempt {
                    tool_name,
                    tool_call_id,
                });
            }
            _ => {}
        }
    }

    if let Some(text) = sanitize_activity_summary_candidate_text([streamed_text.as_str()]) {
        return Ok(AgentActivitySummary {
            text,
            generated_at_ms: now_ms(),
            source_from_seq: request.source_from_seq,
            source_through_seq: request.source_through_seq,
        });
    }

    let attempted_tool_labels = activity_summary_attempted_tool_labels(&attempted_tools);
    tracing::warn!(
        summary_agent_id = %request.summary_agent_id,
        backend_kind = ?request.backend_kind,
        cost_hint = ?SpawnCostHint::Low,
        prompt_len = logged_prompt_len,
        target_workspace_root_count,
        chat_event_count,
        stream_delta_count,
        stream_end_without_usable_text_count,
        backend_error = ?backend_error.as_deref(),
        attempted_tool_count = attempted_tools.len(),
        attempted_tools = %attempted_tool_labels,
        "agent activity summary generator ended without usable assistant text"
    );
    Err(activity_summary_no_usable_text_error(
        backend_error.as_deref(),
        &attempted_tools,
    ))
}

#[derive(Debug)]
struct ActivitySummaryToolAttempt {
    tool_name: String,
    tool_call_id: String,
}

fn sanitize_activity_summary_candidate_text<'a>(
    candidates: impl IntoIterator<Item = &'a str>,
) -> Option<String> {
    candidates.into_iter().find_map(|candidate| {
        if candidate.trim().is_empty() {
            return None;
        }
        sanitize_activity_summary_text(candidate).ok()
    })
}

fn activity_summary_no_usable_text_error(
    backend_error: Option<&str>,
    attempted_tools: &[ActivitySummaryToolAttempt],
) -> String {
    let mut message =
        "agent activity summary generator produced no usable assistant text".to_owned();
    if let Some(error) = backend_error
        .map(str::trim)
        .filter(|error| !error.is_empty())
    {
        message.push_str(": backend error: ");
        message.push_str(error);
    }
    if !attempted_tools.is_empty() {
        let attempted_tool_labels = activity_summary_attempted_tool_labels(attempted_tools);
        message.push_str("; attempted ");
        message.push_str(&attempted_tools.len().to_string());
        message.push_str(" tool call(s)");
        if !attempted_tool_labels.is_empty() {
            message.push_str(": ");
            message.push_str(&attempted_tool_labels);
        }
    }
    message
}

fn activity_summary_attempted_tool_labels(
    attempted_tools: &[ActivitySummaryToolAttempt],
) -> String {
    attempted_tools
        .iter()
        .map(|attempt| {
            if attempt.tool_call_id.trim().is_empty() {
                attempt.tool_name.clone()
            } else {
                format!("{} ({})", attempt.tool_name, attempt.tool_call_id)
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Type-erased backend handle for agent input and acknowledged settings edits.
trait BackendSender: Send + Sync + 'static {
    fn compaction_capability(&self) -> crate::backend::BackendCompactionCapability;
    fn begin_compaction<'a>(
        &'a self,
        request: crate::backend::BackendCompactionRequest,
    ) -> Pin<
        Box<dyn std::future::Future<Output = crate::backend::BackendCompactionStart> + Send + 'a>,
    >;
    fn send_with_outcome<'a>(
        &'a self,
        input: AgentInput,
    ) -> Pin<Box<dyn std::future::Future<Output = SendOutcome> + Send + 'a>>;
    fn update_session_settings<'a>(
        &'a mut self,
        payload: protocol::SetSessionSettingsPayload,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>>;
    fn interrupt<'a>(&'a self) -> Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>>;
    fn shutdown(self: Box<Self>) -> Pin<Box<dyn std::future::Future<Output = ()> + Send>>;
    #[cfg(feature = "test-support")]
    fn mock_control(&self) -> Option<crate::backend::mock::MockControl>;
}

impl<B: Backend> BackendSender for B {
    fn compaction_capability(&self) -> crate::backend::BackendCompactionCapability {
        Backend::compaction_capability(self)
    }

    fn begin_compaction<'a>(
        &'a self,
        request: crate::backend::BackendCompactionRequest,
    ) -> Pin<
        Box<dyn std::future::Future<Output = crate::backend::BackendCompactionStart> + Send + 'a>,
    > {
        Box::pin(Backend::begin_compaction(self, request))
    }

    fn send_with_outcome<'a>(
        &'a self,
        input: AgentInput,
    ) -> Pin<Box<dyn std::future::Future<Output = SendOutcome> + Send + 'a>> {
        Box::pin(Backend::send_with_outcome(self, input))
    }

    fn update_session_settings<'a>(
        &'a mut self,
        payload: protocol::SetSessionSettingsPayload,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(Backend::update_session_settings(self, payload))
    }

    fn interrupt<'a>(&'a self) -> Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>> {
        Box::pin(Backend::interrupt(self))
    }

    fn shutdown(self: Box<Self>) -> Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        Box::pin(async move {
            Backend::shutdown(*self).await;
        })
    }

    #[cfg(feature = "test-support")]
    fn mock_control(&self) -> Option<crate::backend::mock::MockControl> {
        Backend::mock_control(self)
    }
}

async fn prepare_backend_handle_for_adoption(
    handle: crate::backend::PreparedBackendHandle,
    start: &AgentStartPayload,
    sub_agent_context: &HostSubAgentEmitterContext,
) -> Result<BackendHandle, String> {
    let emitter = || {
        Arc::new(
            sub_agent_context
                .clone()
                .emitter(start.agent_id.clone(), start.workspace_roots.clone()),
        )
    };
    match handle {
        crate::backend::PreparedBackendHandle::Tycode(backend) => Ok(backend),
        crate::backend::PreparedBackendHandle::Acp(backend) => Ok(backend),
        crate::backend::PreparedBackendHandle::Claude(backend) => {
            backend.set_subagent_emitter(emitter()).await;
            Ok(backend)
        }
        crate::backend::PreparedBackendHandle::Codex(backend) => {
            if let Err(error) = backend.set_subagent_emitter(emitter()).await {
                Backend::shutdown(*backend).await;
                return Err(format!(
                    "failed to install Codex sub-agent emitter on prepared binding: {error}"
                ));
            }
            Ok(backend)
        }
        crate::backend::PreparedBackendHandle::Antigravity(backend) => Ok(backend),
        crate::backend::PreparedBackendHandle::Hermes(backend) => {
            backend.set_subagent_emitter(emitter()).await;
            Ok(backend)
        }
        crate::backend::PreparedBackendHandle::Mock { backend, .. } => {
            backend.set_subagent_emitter(emitter()).await;
            Ok(backend)
        }
    }
}

/// Spawn the correct backend based on `backend_kind`.
/// Return the live backend session ID. Some backends mint Tyde-owned IDs for non-resumable sessions.
async fn spawn_backend(
    agent_id: &AgentId,
    backend_kind: BackendKind,
    workspace_roots: Vec<String>,
    config: BackendSpawnConfig,
    initial_input: SendMessagePayload,
    sub_agent_context: HostSubAgentEmitterContext,
    antigravity_conversations_dir: Option<PathBuf>,
) -> BackendSpawnResult {
    match backend_kind {
        BackendKind::Tycode => {
            let (b, events) = TycodeBackend::spawn(workspace_roots, config, initial_input).await?;
            let session_id = Backend::session_id(&b);
            Ok((Box::new(b), events, session_id))
        }
        BackendKind::Acp => {
            let (b, events) = KiroBackend::spawn(workspace_roots, config, initial_input).await?;
            let session_id = Backend::session_id(&b);
            Ok((Box::new(b), events, session_id))
        }
        BackendKind::Claude => {
            let emitter =
                Arc::new(sub_agent_context.emitter(agent_id.clone(), workspace_roots.clone()));
            let (b, events) = ClaudeBackend::spawn_with_subagent_emitter(
                workspace_roots,
                config,
                initial_input,
                emitter,
            )
            .await?;
            let session_id = Backend::session_id(&b);
            Ok((Box::new(b), events, session_id))
        }
        BackendKind::Codex => {
            let emitter =
                Arc::new(sub_agent_context.emitter(agent_id.clone(), workspace_roots.clone()));
            let (b, events) = CodexBackend::spawn_with_subagent_emitter(
                workspace_roots,
                config,
                initial_input,
                emitter,
            )
            .await?;
            let session_id = Backend::session_id(&b);
            Ok((Box::new(b), events, session_id))
        }
        BackendKind::Antigravity => {
            let conversations_dir =
                crate::backend::antigravity::resolve_antigravity_conversations_dir(
                    antigravity_conversations_dir.as_deref(),
                )?;
            let (b, events) = AntigravityBackend::spawn_with_conversations_dir(
                workspace_roots,
                config,
                initial_input,
                conversations_dir,
            )
            .await?;
            let session_id = Backend::session_id(&b);
            Ok((Box::new(b), events, session_id))
        }
        BackendKind::Hermes => {
            let (b, events) =
                HermesBackend::spawn(workspace_roots.clone(), config, initial_input).await?;
            b.set_subagent_emitter(Arc::new(
                sub_agent_context.emitter(agent_id.clone(), workspace_roots),
            ))
            .await;
            let session_id = Backend::session_id(&b);
            Ok((Box::new(b), events, session_id))
        }
    }
}

async fn resume_backend(
    agent_id: &AgentId,
    backend_kind: BackendKind,
    workspace_roots: Vec<String>,
    config: BackendSpawnConfig,
    session_id: SessionId,
    sub_agent_context: HostSubAgentEmitterContext,
    antigravity_conversations_dir: Option<PathBuf>,
) -> BackendResumeResult {
    let (backend, events): (BackendHandle, EventStream) = match backend_kind {
        BackendKind::Tycode => {
            let (b, events) = TycodeBackend::resume(workspace_roots, config, session_id).await?;
            (Box::new(b), events)
        }
        BackendKind::Acp => {
            let (b, events) = KiroBackend::resume(workspace_roots, config, session_id).await?;
            (Box::new(b), events)
        }
        BackendKind::Claude => {
            let (b, events) =
                ClaudeBackend::resume(workspace_roots.clone(), config, session_id.clone()).await?;
            b.set_subagent_emitter(Arc::new(
                sub_agent_context.emitter(agent_id.clone(), workspace_roots),
            ))
            .await;
            (Box::new(b), events)
        }
        BackendKind::Codex => {
            let (b, events) =
                CodexBackend::resume(workspace_roots.clone(), config, session_id.clone()).await?;
            b.set_subagent_emitter(Arc::new(
                sub_agent_context.emitter(agent_id.clone(), workspace_roots),
            ))
            .await
            .map_err(|err| format!("Failed to install Codex sub-agent emitter: {err}"))?;
            (Box::new(b), events)
        }
        BackendKind::Antigravity => {
            let conversations_dir =
                crate::backend::antigravity::resolve_antigravity_conversations_dir(
                    antigravity_conversations_dir.as_deref(),
                )?;
            let (b, events) = AntigravityBackend::resume_with_conversations_dir(
                workspace_roots,
                config,
                session_id,
                conversations_dir,
            )
            .await?;
            (Box::new(b), events)
        }
        BackendKind::Hermes => {
            let (b, events) =
                HermesBackend::resume(workspace_roots.clone(), config, session_id).await?;
            b.set_subagent_emitter(Arc::new(
                sub_agent_context.emitter(agent_id.clone(), workspace_roots),
            ))
            .await;
            (Box::new(b), events)
        }
    };
    Ok((backend, events))
}

async fn fork_backend(
    agent_id: &AgentId,
    backend_kind: BackendKind,
    workspace_roots: Vec<String>,
    config: BackendSpawnConfig,
    from_session_id: SessionId,
    initial_input: SendMessagePayload,
    sub_agent_context: HostSubAgentEmitterContext,
) -> BackendForkResult {
    match backend_kind {
        BackendKind::Tycode => {
            let (b, events) =
                TycodeBackend::fork(workspace_roots, config, from_session_id, initial_input)
                    .await?;
            let session_id = Backend::session_id(&b);
            Ok((Box::new(b), events, session_id))
        }
        BackendKind::Acp => {
            let (b, events) =
                KiroBackend::fork(workspace_roots, config, from_session_id, initial_input).await?;
            let session_id = Backend::session_id(&b);
            Ok((Box::new(b), events, session_id))
        }
        BackendKind::Claude => {
            let (b, events) = ClaudeBackend::fork(
                workspace_roots.clone(),
                config,
                from_session_id,
                initial_input,
            )
            .await?;
            b.set_subagent_emitter(Arc::new(
                sub_agent_context.emitter(agent_id.clone(), workspace_roots),
            ))
            .await;
            let session_id = Backend::session_id(&b);
            Ok((Box::new(b), events, session_id))
        }
        BackendKind::Codex => {
            let (b, events) = CodexBackend::fork(
                workspace_roots.clone(),
                config,
                from_session_id,
                initial_input,
            )
            .await?;
            let session_id = Backend::session_id(&b);
            b.set_subagent_emitter(Arc::new(
                sub_agent_context.emitter(agent_id.clone(), workspace_roots),
            ))
            .await
            .map_err(|err| {
                BackendStartupError::backend_failed(format!(
                    "Failed to install Codex sub-agent emitter: {err}"
                ))
            })?;
            Ok((Box::new(b), events, session_id))
        }
        BackendKind::Antigravity => {
            let (b, events) =
                AntigravityBackend::fork(workspace_roots, config, from_session_id, initial_input)
                    .await?;
            let session_id = Backend::session_id(&b);
            Ok((Box::new(b), events, session_id))
        }
        BackendKind::Hermes => {
            let (b, events) =
                HermesBackend::fork(workspace_roots, config, from_session_id, initial_input)
                    .await?;
            let session_id = Backend::session_id(&b);
            Ok((Box::new(b), events, session_id))
        }
    }
}

fn spawn_mock(
    agent_id: AgentId,
    workspace_roots: Vec<String>,
    config: BackendSpawnConfig,
    initial_input: SendMessagePayload,
    sub_agent_context: HostSubAgentEmitterContext,
    launch: Option<crate::backend::mock::MockLaunch>,
) -> BackendFuture<BackendSpawnResult> {
    Box::pin(async move {
        let (b, events) =
            MockBackend::spawn_with_launch(workspace_roots.clone(), config, initial_input, launch)
                .await?;
        let sid = Backend::session_id(&b);
        b.set_subagent_emitter(Arc::new(
            sub_agent_context.emitter(agent_id, workspace_roots),
        ))
        .await;
        Ok((Box::new(b) as BackendHandle, events, sid))
    })
}

fn resume_mock(
    agent_id: AgentId,
    workspace_roots: Vec<String>,
    session_id: SessionId,
    sub_agent_context: HostSubAgentEmitterContext,
    launch: Option<crate::backend::mock::MockLaunch>,
) -> BackendFuture<BackendResumeResult> {
    Box::pin(async move {
        let (b, events) = MockBackend::resume_with_launch(
            workspace_roots.clone(),
            BackendSpawnConfig::default(),
            session_id.clone(),
            launch,
        )
        .await?;
        b.set_subagent_emitter(Arc::new(
            sub_agent_context.emitter(agent_id, workspace_roots),
        ))
        .await;
        Ok((Box::new(b) as BackendHandle, events))
    })
}

fn fork_mock(
    agent_id: AgentId,
    workspace_roots: Vec<String>,
    config: BackendSpawnConfig,
    from_session_id: SessionId,
    initial_input: SendMessagePayload,
    sub_agent_context: HostSubAgentEmitterContext,
    launch: Option<crate::backend::mock::MockLaunch>,
) -> BackendFuture<BackendForkResult> {
    Box::pin(async move {
        let (b, events) = MockBackend::fork_with_launch(
            workspace_roots.clone(),
            config,
            from_session_id,
            initial_input,
            launch,
        )
        .await?;
        let sid = Backend::session_id(&b);
        b.set_subagent_emitter(Arc::new(
            sub_agent_context.emitter(agent_id, workspace_roots),
        ))
        .await;
        Ok((Box::new(b) as BackendHandle, events, sid))
    })
}

pub(crate) fn spawn_agent_actor(
    agent_id: AgentId,
    start: AgentStartPayload,
    request: ResolvedSpawnRequest,
    runtime: AgentActorRuntimeContext,
) -> (AgentHandle, oneshot::Receiver<Result<SessionId, String>>) {
    let AgentActorRuntimeContext {
        session_store,
        transcript_store,
        host_sub_agent_spawn_tx,
        capacity_tx,
        mut supervisor_settings_rx,
        use_mock_backend: supervisor_use_mock_backend,
        supervisor_compaction_tx,
        session_summary_count_tx,
        review_registry,
        status_handle,
        provider_version,
        antigravity_conversations_dir,
    } = runtime;
    let supervisor_capacity_tx = capacity_tx.clone();
    let sub_agent_context = HostSubAgentEmitterContext {
        host_sub_agent_spawn_tx,
        capacity_tx,
    };
    let compaction_sub_agent_context = sub_agent_context.clone();
    let compaction_capacity_tx = sub_agent_context.capacity_tx.clone();
    let compaction_antigravity_conversations_dir = antigravity_conversations_dir.clone();
    let (tx, mut rx) = mpsc::unbounded_channel::<AgentCommand>();
    // A supervisor follow-up is an ordinary message, so it re-enters through
    // this actor's own mailbox instead of getting a private delivery path.
    let supervisor_kick_tx = tx.clone();
    let accepting_input = Arc::new(AtomicBool::new(false));
    let accepting_input_task = Arc::clone(&accepting_input);
    let closing = Arc::new(AtomicBool::new(false));
    let (startup_tx, startup_rx) = oneshot::channel();
    let (start_tx, start_rx) = watch::channel(start.clone());
    let actor_tx = tx.clone();

    tokio::spawn(async move {
        let ResolvedSpawnRequest {
            parent_session_id,
            backend_kind,
            workspace_roots,
            initial_input,
            cost_hint,
            session_settings,
            session_settings_schema,
            backend_config,
            acp_agent,
            startup_mcp_servers,
            resolved_spawn_config,
            resume_session_id,
            fork_from_session_id,
            startup_warning,
            startup_failure,
            initial_alias,
            use_mock_backend,
            mock_launch,
            ..
        } = request;
        let mut current_start = start.clone();
        let session_resumability_config = resolved_spawn_config.clone();
        let spawn_config = BackendSpawnConfig {
            execution_mode: BackendExecutionMode::Agent,
            cost_hint,
            custom_agent_id: current_start.custom_agent_id.clone(),
            startup_mcp_servers,
            session_settings,
            provider_version: provider_version.clone(),
            antigravity_conversations_dir: (backend_kind == BackendKind::Antigravity)
                .then(|| antigravity_conversations_dir.clone()),
            backend_config: backend_config.clone(),
            acp_agent: acp_agent.clone(),
            resolved_spawn_config: resolved_spawn_config.clone(),
        };
        let initial_cost_hint = spawn_config.cost_hint;
        let initial_session_settings = spawn_config.session_settings.clone();
        let compaction_spawn_config = spawn_config.clone();
        let canonical_stream = format!("/agent/{}", agent_id);
        let mut event_log: Vec<Envelope> = Vec::new();
        let mut latest_output = AgentControlLatestOutput::default();
        let mut replay_state = AgentReplayState::default();
        let mut last_backend_event_at: Option<Instant> = None;
        let mut last_stall_interrupt_at: Option<Instant> = None;
        let mut subscribers: Vec<Stream> = Vec::new();
        let mut active_stream_text = String::new();
        let mut activity_stats = AgentActivityStatsTracker::for_backend(backend_kind);
        let mut activity_event_seq = 0_u64;
        let mut current_session_id = resume_session_id.clone();
        let mut pending_alias = initial_alias;
        let session_schema = session_settings_schema;
        let mut current_session_settings = resolve_backend_session_settings(
            backend_kind,
            &BackendSpawnConfig {
                execution_mode: BackendExecutionMode::Agent,
                cost_hint: initial_cost_hint,
                custom_agent_id: current_start.custom_agent_id.clone(),
                acp_agent: None,
                startup_mcp_servers: Vec::new(),
                session_settings: initial_session_settings,
                provider_version: spawn_config.provider_version.clone(),
                antigravity_conversations_dir: spawn_config.antigravity_conversations_dir.clone(),
                backend_config,
                resolved_spawn_config,
            },
        );
        let persisted_queue = if let Some(session_id) = resume_session_id.as_ref() {
            session_store
                .lock()
                .await
                .get(session_id)
                .map(|record| record.queued_messages)
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let mut queue = persisted_queue
            .into_iter()
            .enumerate()
            .map(|(index, entry)| SequencedQueuedMessage {
                sequence: index as u64 + 1,
                entry,
            })
            .collect::<VecDeque<_>>();
        let mut next_queue_sequence = queue.len() as u64 + 1;
        let mut pending_inputs: VecDeque<AgentInput> = VecDeque::new();
        // Checked deliveries this actor has already acknowledged as accepted
        // and parked in `pending_inputs` behind a gate — either the startup
        // loop or the resume-replay barrier. Two later transitions would
        // otherwise normalize a resumed agent back to completed while that
        // accepted work is still queued, publishing Idle for a message the
        // caller was told was accepted: `record_agent_started(is_resume)` and
        // `publish_resumed_agent_idle` at barrier completion. Both consult this
        // count, and both run before the gate lifts and the queue drains, so it
        // is never decremented.
        //
        // This is deliberately checked-delivery specific rather than
        // `pending_inputs.is_empty()`: ordinary fire-and-forget `SendInput`
        // queues the same way and its resume behavior must not change.
        let mut acknowledged_gated_deliveries: usize = 0;
        let mut pending_name_commands = VecDeque::new();
        assert!(
            resume_session_id.is_none() || fork_from_session_id.is_none(),
            "spawn request cannot both resume and fork a session"
        );
        let starts_with_initial_turn = resume_session_id.is_none();
        let is_resume = resume_session_id.is_some();
        let fork_source_session_id = fork_from_session_id.clone();
        let resume_uses_authoritative_transcript = match resume_session_id.as_ref() {
            Some(session_id) => transcript_is_authoritative(&transcript_store, session_id).await,
            None => false,
        };

        #[cfg(feature = "test-support")]
        let startup_gate_name = current_start.name.clone();
        let mut startup_future = Box::pin(async {
            #[cfg(feature = "test-support")]
            wait_for_startup_completion_test_gate(&startup_gate_name).await;
            let mcp_config_validation = if use_mock_backend {
                Ok(())
            } else {
                validate_startup_mcp_configuration(&spawn_config.startup_mcp_servers).await
            };
            eprintln!(
                "TYDE MCP CONFIG VALIDATION agent={} result={:?}",
                agent_id, mcp_config_validation
            );
            let startup_result: Result<
                (
                    BackendHandle,
                    EventStream,
                    SessionId,
                    Option<SendMessagePayload>,
                ),
                AgentStartupFailure,
            > = if let Err(error) = mcp_config_validation {
                Err(AgentStartupFailure::backend_failed(error))
            } else if let Some(err) = startup_failure {
                Err(err)
            } else {
                match resume_session_id {
                    Some(session_id) => {
                        let resumed = if use_mock_backend {
                            resume_mock(
                                agent_id.clone(),
                                workspace_roots.clone(),
                                session_id.clone(),
                                sub_agent_context.clone(),
                                mock_launch,
                            )
                            .await
                        } else {
                            resume_backend(
                                &agent_id,
                                backend_kind,
                                workspace_roots.clone(),
                                spawn_config.clone(),
                                session_id.clone(),
                                sub_agent_context.clone(),
                                Some(antigravity_conversations_dir.clone()),
                            )
                            .await
                        };
                        resumed
                            .map(|(backend, events)| (backend, events, session_id, initial_input))
                            .map_err(AgentStartupFailure::backend_failed)
                    }
                    None => {
                        if let Some(from_session_id) = fork_from_session_id {
                            let first_input =
                                initial_input.expect("fork spawn requires initial_input");
                            let forked = if use_mock_backend {
                                fork_mock(
                                    agent_id.clone(),
                                    workspace_roots.clone(),
                                    spawn_config,
                                    from_session_id,
                                    first_input,
                                    sub_agent_context.clone(),
                                    mock_launch,
                                )
                                .await
                            } else {
                                fork_backend(
                                    &agent_id,
                                    backend_kind,
                                    workspace_roots.clone(),
                                    spawn_config,
                                    from_session_id,
                                    first_input,
                                    sub_agent_context.clone(),
                                )
                                .await
                            };
                            forked
                                .map(|(backend, events, session_id)| {
                                    (backend, events, session_id, None)
                                })
                                .map_err(AgentStartupFailure::from)
                        } else {
                            let first_input =
                                initial_input.expect("new spawn requires initial_input");
                            let spawned = if use_mock_backend {
                                spawn_mock(
                                    agent_id.clone(),
                                    workspace_roots.clone(),
                                    spawn_config,
                                    first_input,
                                    sub_agent_context.clone(),
                                    mock_launch,
                                )
                                .await
                            } else {
                                spawn_backend(
                                    &agent_id,
                                    backend_kind,
                                    workspace_roots.clone(),
                                    spawn_config,
                                    first_input,
                                    sub_agent_context,
                                    Some(antigravity_conversations_dir),
                                )
                                .await
                            };
                            spawned
                                .map(|(backend, events, session_id)| {
                                    (backend, events, session_id, None)
                                })
                                .map_err(AgentStartupFailure::backend_failed)
                        }
                    }
                }
            };
            #[cfg(feature = "test-support")]
            if startup_result.is_ok() {
                wait_for_startup_backend_ready_test_gate(&startup_gate_name).await;
            }
            startup_result
        });
        let startup_cancellation_supported = backend_startup_drop_cancels_workers(backend_kind);
        let mut pending_startup_attaches: Vec<(Stream, oneshot::Sender<bool>)> = Vec::new();
        let startup_result = loop {
            match next_agent_startup_event(
                startup_future.as_mut(),
                &mut rx,
                startup_cancellation_supported,
            )
            .await
            {
                AgentStartupEvent::Completed(result) => break result,
                AgentStartupEvent::Command(command) => {
                    let Some(command) = *command else {
                        return;
                    };
                    match command {
                        AgentCommand::Interrupt { reply } => {
                            tracing::debug!(
                                agent_id = %current_start.agent_id,
                                ?backend_kind,
                                "interrupting agent during backend startup"
                            );
                            let _ = reply.send(InterruptOutcome::Interrupted);
                            break Err(AgentStartupFailure::internal("agent startup interrupted"));
                        }
                        AgentCommand::Close { reply } => {
                            tracing::debug!(
                                agent_id = %current_start.agent_id,
                                ?backend_kind,
                                "closing agent during backend startup"
                            );
                            accepting_input_task.store(false, Ordering::SeqCst);
                            status_handle
                                .update(|status| {
                                    status.terminated = true;
                                    status.is_thinking = false;
                                    status.turn_completed = true;
                                    status.pending_user_response = None;
                                    status.activity_counter =
                                        status.activity_counter.saturating_add(1);
                                })
                                .await;
                            for (_, attach_reply) in std::mem::take(&mut pending_startup_attaches) {
                                let _ = attach_reply.send(true);
                            }
                            let _ = reply.send(());
                            let _ = startup_tx.send(Err("agent startup closed".to_owned()));
                            return;
                        }
                        AgentCommand::Attach { stream, reply } => {
                            tracing::debug!(
                                agent_id = %current_start.agent_id,
                                stream = %stream.path(),
                                "deferring agent stream attachment until startup bootstrap is available"
                            );
                            pending_startup_attaches.push((stream, reply));
                        }
                        AgentCommand::ReadOutput { reply, .. } => {
                            let _ = reply.send(Vec::new());
                        }
                        AgentCommand::ReadLatestOutput { reply } => {
                            let _ = reply.send(Ok(latest_output.output().clone()));
                        }
                        AgentCommand::FetchSessionHistory {
                            before_seq,
                            limit,
                            reply,
                        } => {
                            let _ = reply
                                .send(session_history_window(&event_log, before_seq, limit, None));
                        }
                        AgentCommand::ReadActivityHistory { reply, .. } => {
                            let _ = reply.send(AgentActivityHistorySnapshot {
                                rendered: String::new(),
                                from_seq: None,
                                through_seq: None,
                                event_count: 0,
                                active_stream_included: false,
                            });
                        }
                        AgentCommand::ReadUsageSnapshot { reply } => {
                            let _ = reply.send(agent_usage_snapshot_from_tracker(
                                &current_start,
                                &activity_stats,
                            ));
                        }
                        command @ AgentCommand::SetName { .. } => {
                            pending_name_commands.push_back(command);
                            #[cfg(feature = "test-support")]
                            notify_startup_name_stashed_test_gate(&current_start.name);
                        }
                        command @ AgentCommand::ApplyGeneratedName { .. } => {
                            pending_name_commands.push_back(command);
                            #[cfg(feature = "test-support")]
                            notify_startup_name_stashed_test_gate(&current_start.name);
                        }
                        AgentCommand::Compact { reply, .. } => {
                            let _ = reply.send(Err("agent backend is starting".to_owned()));
                        }
                        AgentCommand::CompactIfInactive {
                            accepted, reply, ..
                        } => {
                            let error = "agent backend is starting".to_owned();
                            let _ = accepted.send(Err(error.clone()));
                            let _ = reply.send(Err(error));
                        }
                        AgentCommand::ReadCompactionCapability { reply } => {
                            let _ =
                                reply.send(crate::backend::BackendCompactionCapability::default());
                        }
                        AgentCommand::ReadRequestedCompactionRoute { reply, .. } => {
                            let _ = reply.send(Err("agent backend is starting".to_owned()));
                        }
                        AgentCommand::RequestContextCompaction { reply, .. } => {
                            let _ = reply.send(Err("agent backend is starting".to_owned()));
                        }
                        AgentCommand::ContextCompactionFallbackPrepared { result, .. } => {
                            if let Ok(prepared) = result {
                                prepared.binding.backend.shutdown().await;
                            }
                        }
                        AgentCommand::ContextCompactionTerminal { .. }
                        | AgentCommand::RetryContextCompaction { .. }
                        | AgentCommand::ContextCompactionBarrierExpired { .. } => {}
                        AgentCommand::ReleaseCompaction { reply } => {
                            let _ = reply.send(());
                        }
                        AgentCommand::SendInput(input) => {
                            pending_inputs.push_back(input);
                        }
                        AgentCommand::DeliverMessage { payload, reply } => {
                            // Accepted: a starting agent is already active
                            // (`started` is false), and the message is queued
                            // for dispatch once the backend is up. Rejecting it
                            // here would make spawn-then-send racy for no gain.
                            acknowledged_gated_deliveries =
                                acknowledged_gated_deliveries.saturating_add(1);
                            pending_inputs.push_back(AgentInput::SendMessage(payload));
                            let _ = reply.send(Ok(()));
                        }
                        AgentCommand::ResumeReplayBarrier { .. } => {}
                        #[cfg(feature = "test-support")]
                        AgentCommand::ForceBackendShutdownForConformance { reply } => {
                            let _ = reply.send(true);
                            break Err(AgentStartupFailure::backend_failed(
                                "agent backend owner died during startup",
                            ));
                        }
                        // The backend does not exist yet; the fixture always
                        // reads the control after consuming `AgentStart`,
                        // which is only published once startup completed.
                        #[cfg(feature = "test-support")]
                        AgentCommand::ReadMockControl { reply } => {
                            let _ = reply.send(None);
                        }
                    }
                }
            }
        };
        drop(startup_future);
        for command in pending_name_commands {
            let _ = actor_tx.send(command);
        }

        let (backend, mut events, actor_session_id, initial_follow_up) = match startup_result {
            Ok(result) => result,
            Err(err) => {
                eprintln!(
                    "TYDE STARTUP FAILURE agent={} pending_attaches={} message={}",
                    current_start.agent_id,
                    pending_startup_attaches.len(),
                    err.message
                );
                let _ = startup_tx.send(Err(err.message.clone()));
                let payload = AgentErrorPayload {
                    agent_id: current_start.agent_id.clone(),
                    code: err.code,
                    message: format!("failed to start agent backend: {}", err.message),
                    fatal: true,
                };
                append_event(
                    &canonical_stream,
                    &mut event_log,
                    &mut subscribers,
                    FrameKind::AgentStart,
                    &current_start,
                )
                .await;
                upsert_activity_stats_snapshot(
                    &canonical_stream,
                    &mut event_log,
                    &mut subscribers,
                    &current_start.agent_id,
                    activity_stats.snapshot(),
                )
                .await;
                enter_terminal_failure(
                    TerminalFailureContext {
                        accepting_input: &accepting_input_task,
                        status_handle: &status_handle,
                        canonical_stream: &canonical_stream,
                        event_log: &mut event_log,
                        replay_state: &mut replay_state,
                        subscribers: &mut subscribers,
                        queue: &mut queue,
                        session_store: &session_store,
                        compaction: None,
                    },
                    &payload,
                )
                .await;
                flush_pending_agent_attaches(
                    &event_log,
                    Some(&replay_state),
                    &mut latest_output,
                    &mut subscribers,
                    &mut pending_startup_attaches,
                    &status_handle,
                )
                .await;
                park_terminal_agent(
                    &session_store,
                    &transcript_store,
                    current_session_id.as_ref(),
                    &mut pending_alias,
                    &mut current_start,
                    &start_tx,
                    &mut event_log,
                    &mut latest_output,
                    &mut subscribers,
                    &mut pending_inputs,
                    &mut rx,
                )
                .await;
                return;
            }
        };
        tracing::debug!(
            agent_id = %current_start.agent_id,
            ?backend_kind,
            "agent backend startup completed"
        );
        let mut backend = Some(backend);
        let mut in_turn = starts_with_initial_turn;
        let mut idle_transition_armed = false;
        // Last typing value the backend itself emitted. While this is true the
        // backend has an open turn, so a generic Error card is a mid-turn
        // diagnostic, not a terminal signal — ending the turn on it desyncs
        // this actor from the still-streaming backend, and every later event
        // of that turn is then dropped as a stream identity violation. The
        // error-ends-turn heuristic below only fires once the backend has gone
        // quiet (never emitted typing(true), or already emitted typing(false))
        // without a proper idle marker. Interrupted tool completions are not
        // gated on this: they are a narrow, deliberately terminal marker even
        // while typing is on.
        let mut backend_typing = false;
        let mut pending_tool_response_ids: HashSet<String> = HashSet::new();
        let mut open_tool_call_ids: HashSet<String> = HashSet::new();
        let mut open_tool_requests: HashMap<String, protocol::ToolRequest> = HashMap::new();
        let mut completed_tool_call_ids = if resume_uses_authoritative_transcript {
            load_authoritative_completed_tool_call_ids(&transcript_store, &actor_session_id).await
        } else {
            HashSet::new()
        };
        let mut active_agent_await_ids: HashSet<String> = HashSet::new();
        let mut lifecycle = ActorLifecycle::Running;
        let mut close_reply: Option<oneshot::Sender<()>> = None;
        let mut close_deadline: Option<tokio::time::Instant> = None;
        let mut active_compaction: Option<ActiveCompaction> = None;
        let mut context_compaction: Option<CompactionFlight> = None;
        // Observations already accounted for by a requested operation's marker.
        // The flight alone cannot carry this: the terminal result takes it, and
        // the backend's observation of the same compaction arrives afterwards on
        // a different channel with nothing left to correlate against. Bounded
        // because a long-lived agent compacts many times; an entry that is never
        // consumed can only ever suppress the one observation it names.
        let mut correlated_compaction_observations: VecDeque<CompactionObservationId> =
            VecDeque::new();
        let mut compaction_blocked = false;
        current_session_id = Some(actor_session_id.clone());
        register_transcript_session(&canonical_stream, &actor_session_id, &transcript_store);
        current_start.session_id = Some(actor_session_id.clone());
        let _ = start_tx.send(current_start.clone());
        let mut resume_replay_gate_pending = false;
        let mut deferred_authoritative_resume_events = Vec::new();
        let mut pending_resume_attaches: Vec<(Stream, oneshot::Sender<bool>)> = Vec::new();
        let mut resume_replay_barrier_task = None;
        if is_resume && let Some(barrier_rx) = events.take_resume_replay_complete() {
            resume_replay_gate_pending = true;
            pending_resume_attaches.append(&mut pending_startup_attaches);
            resume_replay_barrier_task = Some(spawn_resume_replay_barrier_task(
                actor_tx.clone(),
                barrier_rx,
                current_start.agent_id.clone(),
            ));
        }
        if let Err(err) = persist_agent_session(
            &session_store,
            &actor_session_id,
            parent_session_id,
            &current_start,
            &current_session_settings,
            &session_resumability_config,
            &mut pending_alias,
        )
        .await
        {
            tracing::error!(
                agent_id = %current_start.agent_id,
                session_id = %actor_session_id,
                error = %err,
                "failed to persist agent session startup state"
            );
        }
        let mut persisted_resume_task_list = if is_resume {
            session_store.lock().await.get_task_list(&actor_session_id)
        } else {
            None
        };
        let _ = startup_tx.send(Ok(actor_session_id.clone()));
        accepting_input_task.store(!resume_replay_gate_pending, Ordering::SeqCst);
        let has_acknowledged_gated_deliveries = acknowledged_gated_deliveries > 0;
        status_handle
            .update(|s| {
                record_agent_started(s, is_resume);
                if has_acknowledged_gated_deliveries {
                    // A resume normalizes to completed here. That would publish
                    // Idle for a message this actor already acknowledged as
                    // accepted, so re-assert the active turn it is queued for.
                    s.is_thinking = true;
                    s.turn_completed = false;
                }
            })
            .await;
        append_event(
            &canonical_stream,
            &mut event_log,
            &mut subscribers,
            FrameKind::AgentStart,
            &current_start,
        )
        .await;
        if resume_uses_authoritative_transcript
            && let Err(error) = seed_existing_transcript_history(
                &transcript_store,
                &actor_session_id,
                &canonical_stream,
                &mut event_log,
            )
            .await
        {
            tracing::error!(
                session_id = %actor_session_id,
                %error,
                "failed to seed authoritative resume transcript"
            );
        }
        if let Some(source_session_id) = fork_source_session_id.as_ref()
            && let Err(error) = seed_fork_transcript_history(
                &transcript_store,
                source_session_id,
                &actor_session_id,
                &canonical_stream,
                &mut event_log,
            )
            .await
        {
            tracing::error!(
                source_session_id = %source_session_id,
                fork_session_id = %actor_session_id,
                %error,
                "failed to seed fork transcript history"
            );
        }
        upsert_activity_stats_snapshot(
            &canonical_stream,
            &mut event_log,
            &mut subscribers,
            &current_start.agent_id,
            activity_stats.snapshot(),
        )
        .await;
        let backend_capability = backend
            .as_ref()
            .expect("backend must exist after startup")
            .compaction_capability();
        upsert_context_compaction_capability(
            &canonical_stream,
            &mut event_log,
            &mut subscribers,
            &ContextCompactionCapabilityPayload {
                agent_id: current_start.agent_id.clone(),
                logical_session_id: actor_session_id.clone(),
                availability: crate::host::requested_compaction_availability(
                    &backend_capability,
                    &crate::host::CompactionRoutingPolicy::default(),
                    transcript_is_authoritative(&transcript_store, &actor_session_id).await,
                ),
            },
        )
        .await;
        if let Some(warning) = startup_warning {
            append_event(
                &canonical_stream,
                &mut event_log,
                &mut subscribers,
                FrameKind::AgentError,
                &AgentErrorPayload {
                    agent_id: current_start.agent_id.clone(),
                    code: AgentErrorCode::Internal,
                    message: warning,
                    fatal: false,
                },
            )
            .await;
        }
        append_event(
            &canonical_stream,
            &mut event_log,
            &mut subscribers,
            FrameKind::SessionSettings,
            &SessionSettingsPayload {
                values: current_session_settings.clone(),
            },
        )
        .await;
        update_queued_messages_snapshot(
            &canonical_stream,
            &mut event_log,
            &mut subscribers,
            &queue,
            &session_store,
        )
        .await;
        if !resume_replay_gate_pending {
            flush_pending_agent_attaches(
                &event_log,
                Some(&replay_state),
                &mut latest_output,
                &mut subscribers,
                &mut pending_startup_attaches,
                &status_handle,
            )
            .await;
        }

        let mut initial_follow_up = initial_follow_up.filter(|input| {
            !input.message.trim().is_empty()
                || input
                    .images
                    .as_ref()
                    .is_some_and(|images| !images.is_empty())
        });
        if !resume_replay_gate_pending
            && let Some(input) = initial_follow_up.take()
            && !send_initial_follow_up_or_park(
                input,
                InitialFollowUpContext {
                    backend: &mut backend,
                    in_turn: &mut in_turn,
                    idle_transition_armed: &mut idle_transition_armed,
                    session_store: &session_store,
                    transcript_store: &transcript_store,
                    current_session_id: current_session_id.as_ref(),
                    pending_alias: &mut pending_alias,
                    current_start: &mut current_start,
                    start_tx: &start_tx,
                    accepting_input: &accepting_input_task,
                    status_handle: &status_handle,
                    canonical_stream: &canonical_stream,
                    event_log: &mut event_log,
                    latest_output: &mut latest_output,
                    replay_state: &mut replay_state,
                    subscribers: &mut subscribers,
                    queue: &mut queue,
                    next_queue_sequence: &mut next_queue_sequence,
                    pending_inputs: &mut pending_inputs,
                    rx: &mut rx,
                },
            )
            .await
        {
            abort_resume_replay_barrier_task(&mut resume_replay_barrier_task);
            return;
        }
        let mut supervisor_state = supervisor::SupervisorState::new(
            &status_handle.snapshot().await,
            supervisor_settings_rx.borrow().settings,
            Instant::now(),
        );
        let mut last_supervisor_settings = *supervisor_settings_rx.borrow();
        // Carries the activity counter the verdict was launched under, which is
        // the whole staleness check: if it still matches, nothing happened in
        // this conversation while the call was out.
        let (supervisor_verdict_tx, mut supervisor_verdict_rx) = mpsc::unbounded_channel::<(
            u64,
            Result<supervisor::SupervisionVerdict, supervisor::SupervisionFailure>,
        )>();
        loop {
            latest_output
                .observe_event_log(&event_log)
                .expect("typed agent replay log must project latest output");
            // The loop turns whenever a backend event or command lands, which
            // is exactly when this agent's status can have changed, so the
            // supervisor sees every transition without polling anything.
            let supervisor_settings = *supervisor_settings_rx.borrow();
            if supervisor_settings != last_supervisor_settings {
                supervisor_state.apply_settings_change(
                    last_supervisor_settings.settings,
                    supervisor_settings.settings,
                    Instant::now(),
                );
                last_supervisor_settings = supervisor_settings;
            }
            let supervisor_status = status_handle.snapshot().await;
            supervisor_state.observe(
                &supervisor_status,
                supervisor_settings.settings,
                &event_log,
                active_compaction.is_some() || context_compaction.is_some(),
                Instant::now(),
            );
            let supervisor_deadline = supervisor_state
                .next_deadline(supervisor_settings.settings, supervisor_settings.epoch);
            let stall_timeout = Duration::from_secs(u64::from(
                supervisor_settings.settings.stall_timeout_seconds,
            ));
            // The turn's own start counts as progress, so a backend that goes
            // silent immediately is measured from when it began rather than
            // from an older event. One interrupt per window: a backend that
            // swallows the first gets another a full window later, not a loop.
            let stall_deadline = (supervisor_settings.settings.enabled
                && supervisor_settings.settings.stall_timeout_enabled
                && in_turn)
                .then(|| {
                    [
                        supervisor_status.turn_started_at,
                        last_backend_event_at,
                        last_stall_interrupt_at,
                    ]
                    .into_iter()
                    .flatten()
                    .max()
                    .and_then(|at| at.checked_add(stall_timeout))
                })
                .flatten();
            let supervisor_sleep = tokio::time::sleep_until(
                supervisor_deadline
                    .unwrap_or_else(|| Instant::now() + Duration::from_secs(86_400))
                    .into(),
            );
            let stall_sleep = tokio::time::sleep_until(
                stall_deadline
                    .unwrap_or_else(|| Instant::now() + Duration::from_secs(86_400))
                    .into(),
            );
            tokio::pin!(supervisor_sleep);
            tokio::pin!(stall_sleep);
            tokio::select! {
                _ = &mut supervisor_sleep, if supervisor_deadline.is_some() => {
                    let now = Instant::now();
                    let action = supervisor_state.due_action(
                        supervisor_settings.settings,
                        supervisor_settings.epoch,
                        now,
                    );
                    // Everything the host used to re-verify over a round trip
                    // — that the agent is still idle, that nothing is queued,
                    // that the actor is not closing — is just local state here.
                    let idle = !supervisor_status.terminated
                        && !supervisor_status.is_active()
                        && !supervisor_status.is_user_response_pending()
                        && !matches!(lifecycle, ActorLifecycle::Closing)
                        && !in_turn
                        && queue.is_empty();
                    if let supervisor::SupervisorAction::LaunchVerdict { attempts_started } = action {
                        let context = supervisor::supervision_context_snapshot(&event_log);
                        let record = match current_session_id.as_ref() {
                            Some(session_id) => session_store.lock().await.get(session_id),
                            None => None,
                        };
                        let allowed = supervisor::supervision_record_allows_action(
                            record.as_ref(),
                            &context,
                            supervisor::SupervisionAction::Verdict,
                        );
                        // Spending a paid call with no follow-up left to send
                        // would buy nothing, so the budget gates the verdict
                        // rather than only the kick it might produce.
                        let kick_budget_left = context.kicks_since_user_message
                            < u32::from(supervisor_settings.settings.max_kicks_per_task.max(1));
                        match context.last_user_message.clone() {
                            Some(last_user_message)
                                if idle
                                    && allowed
                                    && kick_budget_left
                                    && !context.cancelled_since_user_message =>
                            {
                                let task_list = match current_session_id.as_ref() {
                                    Some(session_id) => {
                                        session_store.lock().await.get_task_list(session_id)
                                    }
                                    None => None,
                                };
                                let request = supervisor::GenerateSupervisionVerdictRequest {
                                    verdict_agent_id: AgentId(Uuid::new_v4().to_string()),
                                    backend_kind,
                                    last_user_message,
                                    task_list,
                                    last_assistant_message: context.last_assistant_message.clone(),
                                    last_error: context.last_error_since_user_message.clone(),
                                    stall_interrupted: context.last_turn_was_stall_interrupted,
                                    kicks_so_far: context.kicks_since_user_message,
                                    last_kick_message: context.last_kick_message.clone(),
                                    last_reply_to_kick: context.last_reply_to_kick.clone(),
                                    cost_hint: supervisor_settings.settings.cost_tier.as_cost_hint(),
                                    session_settings: crate::host::hidden_helper_session_settings(
                                        backend_kind,
                                        record
                                            .as_ref()
                                            .and_then(|record| record.session_settings.as_ref()),
                                    ),
                                    use_mock_backend: supervisor_use_mock_backend,
                                    capacity_tx: supervisor_capacity_tx.clone(),
                                };
                                supervisor_state
                                    .begin_verdict(supervisor_settings.settings, attempts_started);
                                let verdict_tx = supervisor_verdict_tx.clone();
                                let launched_at = supervisor_status.activity_counter;
                                tokio::spawn(async move {
                                    let result = match tokio::time::timeout(
                                        supervisor::SUPERVISION_GENERATION_TIMEOUT,
                                        supervisor::generate_supervision_verdict(request),
                                    )
                                    .await
                                    {
                                        Ok(result) => result,
                                        Err(_) => Err(supervisor::SupervisionFailure {
                                            kind: supervisor::SupervisionFailureKind::Timeout,
                                            message: "supervision verdict timed out".to_owned(),
                                        }),
                                    };
                                    let _ = verdict_tx.send((launched_at, result));
                                });
                            }
                            _ => supervisor_state.settle(now),
                        }
                    }
                    if action == supervisor::SupervisorAction::RequestCompaction {
                        let context = supervisor::supervision_context_snapshot(&event_log);
                        let record = match current_session_id.as_ref() {
                            Some(session_id) => session_store.lock().await.get(session_id),
                            None => None,
                        };
                        let over_threshold =
                            context.current_context_input_tokens.is_some_and(|current| {
                                current
                                    > supervisor_settings.settings.auto_compact_min_context_tokens
                            });
                        if idle
                            && over_threshold
                            && supervisor::supervision_record_allows_action(
                                record.as_ref(),
                                &context,
                                supervisor::SupervisionAction::AutoCompaction,
                            )
                        {
                            supervisor_state.begin_compaction(now);
                            let _ = supervisor_compaction_tx.send(
                                crate::host::SupervisorCompactionRequest {
                                    agent_id: current_start.agent_id.clone(),
                                    activity_counter: supervisor_status.activity_counter,
                                    settings_epoch: supervisor_settings.epoch,
                                },
                            );
                        } else {
                            // Evaluated for this settings epoch: a threshold that
                            // is not met is checked once, not every tick.
                            supervisor_state.mark_compaction_evaluated(supervisor_settings.epoch);
                        }
                    }
                }
                // A settings edit changes what the next deadline should be, and
                // may arm supervision that was switched off entirely, so it has
                // to wake the loop rather than wait for unrelated traffic.
                Ok(()) = supervisor_settings_rx.changed() => {}
                Some((launched_at, result)) = supervisor_verdict_rx.recv() => {
                    let now = Instant::now();
                    let launched_settings = supervisor_state.in_flight_verdict(launched_at);
                    if launched_settings.is_none() {
                        tracing::debug!(
                            agent_id = %current_start.agent_id,
                            "dropping a supervision verdict the conversation moved past"
                        );
                    } else if launched_settings
                        != Some(supervisor::VerdictSettingsFingerprint::from(
                            supervisor_settings.settings,
                        ))
                    {
                        // The user edited the question while the call was out,
                        // so this answer is to the old one.
                        supervisor_state.note_verdict_failure(
                            supervisor::SupervisionRetryReason::SettingsChanged,
                            supervisor_settings.settings,
                            now,
                        );
                    } else {
                        match result {
                            Ok(supervisor::SupervisionVerdict::Continue { message }) => {
                                supervisor_state.settle(now);
                                let payload = SendMessagePayload {
                                    message: format!("{SUPERVISOR_MESSAGE_PREFIX}{message}"),
                                    images: None,
                                    origin: None,
                                    tool_response: None,
                                };
                                let _ = supervisor_kick_tx.send(AgentCommand::SendInput(
                                    AgentInput::SendMessage(payload),
                                ));
                            }
                            Ok(_) => supervisor_state.settle(now),
                            Err(failure) => {
                                let exhausted = if failure.is_retryable() {
                                    supervisor_state.note_verdict_failure(
                                        supervisor::SupervisionRetryReason::Failure(failure.kind),
                                        supervisor_settings.settings,
                                        now,
                                    )
                                } else {
                                    supervisor_state.settle(now);
                                    Some(0)
                                };
                                if let Some(attempts_started) = exhausted {
                                    tracing::warn!(
                                        agent_id = %current_start.agent_id,
                                        error = %failure.message,
                                        "agent supervision gave up on this turn"
                                    );
                                    append_chat_event(
                                        &canonical_stream,
                                        &mut event_log,
                                        &mut subscribers,
                                        &mut replay_state,
                                        &supervisor_failure_warning_event(attempts_started),
                                    )
                                    .await;
                                }
                            }
                        }
                    }
                }
                _ = &mut stall_sleep, if stall_deadline.is_some() => {
                    let now = Instant::now();
                    let stalled = !supervisor_status.terminated
                        && !matches!(lifecycle, ActorLifecycle::Closing)
                        && supervisor_status.is_active()
                        // Waiting on a person is not stalling.
                        && !supervisor_status.is_user_response_pending()
                        && active_compaction.is_none()
                        && !compaction_blocked
                        // Detached background work is real progress that the
                        // stream cannot show.
                        && active_agent_await_ids.is_empty()
                        // Cutting a turn short with no follow-up left to send
                        // would destroy work and offer nothing in return.
                        && supervisor::supervision_context_snapshot(&event_log)
                            .kicks_since_user_message
                            < u32::from(supervisor_settings.settings.max_kicks_per_task.max(1));
                    if stalled {
                        append_chat_event(
                            &canonical_stream,
                            &mut event_log,
                            &mut subscribers,
                            &mut replay_state,
                            &supervisor_stall_interrupt_notice_event(
                                supervisor_settings.settings.stall_timeout_seconds,
                            ),
                        )
                        .await;
                        let interrupted = backend
                            .as_ref()
                            .expect("backend must exist while actor is running")
                            .interrupt()
                            .await;
                        last_stall_interrupt_at = Some(now);
                        if interrupted {
                            tracing::warn!(
                                agent_id = %current_start.agent_id,
                                stall_timeout_seconds =
                                    supervisor_settings.settings.stall_timeout_seconds,
                                "supervisor interrupted a turn that stopped making progress"
                            );
                        } else {
                            let payload = AgentErrorPayload {
                                agent_id: current_start.agent_id.clone(),
                                code: AgentErrorCode::Internal,
                                message: "agent backend does not support interrupt".to_owned(),
                                fatal: false,
                            };
                            append_event(
                                &canonical_stream,
                                &mut event_log,
                                &mut subscribers,
                                FrameKind::AgentError,
                                &payload,
                            )
                            .await;
                        }
                    } else {
                        last_stall_interrupt_at = Some(now);
                    }
                }
                maybe_event = events.recv_backend() => {
                    let Some(event) = maybe_event else {
                        if let Some(compaction) = active_compaction.take() {
                            let _ = compaction
                                .reply
                                .send(Err("agent backend closed during compaction".to_owned()));
                        }
                        if let Some(flight) = context_compaction.take() {
                            let accepted = matches!(
                                flight.state,
                                StoredCompactionState::NativeAccepted
                            ) || flight.terminal_taken;
                            let mutation = if accepted {
                                CompactionMutation::MayHaveMutated
                            } else {
                                CompactionMutation::NotObserved
                            };
                            record_context_compaction_terminal(
                                flight,
                                ContextCompactionTerminalRecord {
                                    accepted,
                                    mutation,
                                    method: None,
                                    metrics: CompactionMetrics::default(),
                                    provider_session_id: None,
                                    status: ContextCompactionTimelineStatus::Failed,
                                    message: Some(
                                        "agent backend closed during context compaction"
                                            .to_owned(),
                                    ),
                                    trusted_post_context_tokens:
                                        accepted.then_some(None),
                                },
                                &session_store,
                                current_session_id
                                    .as_ref()
                                    .expect("live agent must have session_id"),
                                &current_start,
                                &canonical_stream,
                                &mut event_log,
                                &mut replay_state,
                                &mut subscribers,
                                &mut activity_stats,
                                Some(&mut activity_event_seq),
                            )
                            .await;
                        }
                        if resume_replay_gate_pending {
                            event_log.retain(|event| event.kind != FrameKind::ChatEvent);
                            replay_state = AgentReplayState::default();
                            latest_output = AgentControlLatestOutput::default();
                            let payload = AgentErrorPayload {
                                agent_id: current_start.agent_id.clone(),
                                code: AgentErrorCode::BackendFailed,
                                message: "agent backend closed before resume replay completed"
                                    .to_owned(),
                                fatal: true,
                            };
                            terminalize_live_activity(
                                LiveActivityTerminalContext {
                                    canonical_stream: &canonical_stream,
                                    event_log: &mut event_log,
                                    replay_state: &mut replay_state,
                                    subscribers: &mut subscribers,
                                    open_tool_call_ids: &mut open_tool_call_ids,
                                    pending_tool_response_ids: &mut pending_tool_response_ids,
                                    active_agent_await_ids: &mut active_agent_await_ids,
                                },
                                LiveActivityTerminalStatus::Failed,
                                "agent backend closed before resume replay completed",
                            )
                            .await;
                            enter_terminal_failure(
                                TerminalFailureContext {
                                    accepting_input: &accepting_input_task,
                                    status_handle: &status_handle,
                                    canonical_stream: &canonical_stream,
                                    event_log: &mut event_log,
                                    replay_state: &mut replay_state,
                                    subscribers: &mut subscribers,
                                    queue: &mut queue,
                                    session_store: &session_store,
                                    compaction: Some(TerminalCompactionFailureContext {
                                        flight: &mut context_compaction,
                                        session_store: &session_store,
                                        session_id: current_session_id
                                            .as_ref()
                                            .expect("live agent must have session_id"),
                                        start: &current_start,
                                        activity_stats: &mut activity_stats,
                                    }),
                                },
                                &payload,
                            )
                            .await;
                            flush_pending_agent_attaches(
                                &event_log,
                                None,
                                &mut latest_output,
                                &mut subscribers,
                                &mut pending_resume_attaches,
                                &status_handle,
                            )
                            .await;
                            abort_resume_replay_barrier_task(&mut resume_replay_barrier_task);
                            if let Some(backend) = backend.take() {
                                backend.shutdown()
                                    .await;
                            }
                            park_terminal_agent(
                                &session_store,
                                &transcript_store,
                                current_session_id.as_ref(),
                                &mut pending_alias,
                                &mut current_start,
                                &start_tx,
                                &mut event_log,
                                &mut latest_output,
                                &mut subscribers,
                                &mut pending_inputs,
                                &mut rx,
                            )
                            .await;
                            return;
                        }
                        if matches!(lifecycle, ActorLifecycle::Closing) {
                            let reply = close_reply
                                .take()
                                .expect("close requested without pending close reply");
                            if let Some(backend) = backend.take() {
                                backend.shutdown().await;
                            }
                            abort_resume_replay_barrier_task(&mut resume_replay_barrier_task);
                            terminalize_live_activity(
                                LiveActivityTerminalContext {
                                    canonical_stream: &canonical_stream,
                                    event_log: &mut event_log,
                                    replay_state: &mut replay_state,
                                    subscribers: &mut subscribers,
                                    open_tool_call_ids: &mut open_tool_call_ids,
                                    pending_tool_response_ids: &mut pending_tool_response_ids,
                                    active_agent_await_ids: &mut active_agent_await_ids,
                                },
                                LiveActivityTerminalStatus::Stopped,
                                "agent closed",
                            )
                            .await;
                            finish_actor_close(&accepting_input_task, &status_handle, reply).await;
                            return;
                        }
                        let payload = AgentErrorPayload {
                            agent_id: current_start.agent_id.clone(),
                            code: AgentErrorCode::BackendFailed,
                            message: "agent backend closed".to_owned(),
                            fatal: true,
                        };
                        terminalize_live_activity(
                            LiveActivityTerminalContext {
                                canonical_stream: &canonical_stream,
                                event_log: &mut event_log,
                                replay_state: &mut replay_state,
                                subscribers: &mut subscribers,
                                open_tool_call_ids: &mut open_tool_call_ids,
                                pending_tool_response_ids: &mut pending_tool_response_ids,
                                active_agent_await_ids: &mut active_agent_await_ids,
                            },
                            LiveActivityTerminalStatus::Failed,
                            "agent backend closed",
                        )
                        .await;
                        enter_terminal_failure(
                            TerminalFailureContext {
                                accepting_input: &accepting_input_task,
                                status_handle: &status_handle,
                                canonical_stream: &canonical_stream,
                                event_log: &mut event_log,
                                replay_state: &mut replay_state,
                                subscribers: &mut subscribers,
                                queue: &mut queue,
                                session_store: &session_store,
                                compaction: Some(TerminalCompactionFailureContext {
                                    flight: &mut context_compaction,
                                    session_store: &session_store,
                                    session_id: current_session_id
                                        .as_ref()
                                        .expect("live agent must have session_id"),
                                    start: &current_start,
                                    activity_stats: &mut activity_stats,
                                }),
                            },
                            &payload,
                        )
                        .await;
                        park_terminal_agent(
                            &session_store,
                            &transcript_store,
                            current_session_id.as_ref(),
                            &mut pending_alias,
                            &mut current_start,
                            &start_tx,
                            &mut event_log,
                            &mut latest_output,
                            &mut subscribers,
                            &mut pending_inputs,
                            &mut rx,
                        )
                        .await;
                        return;
                    };
                    if resume_replay_gate_pending && resume_uses_authoritative_transcript {
                        // The backend owns this barrier and closes it before it
                        // accepts live conversation work. Chat events on its
                        // pre-barrier side are provider replay even when their
                        // normalized shape differs from the authoritative
                        // journal. Compaction observations on this side are
                        // replay too: restoring them as live events duplicates
                        // the authoritative timeline with provider-local ids.
                        match event {
                            event @ BackendEvent::ModelRequestTokenUsage(_) => {
                                deferred_authoritative_resume_events.push(event);
                            }
                            BackendEvent::Chat(_) | BackendEvent::Compaction(_) => {}
                        }
                        continue;
                    }
                    let mut event = match event {
                        BackendEvent::Chat(event) => event,
                        BackendEvent::ModelRequestTokenUsage(usage) => {
                            let source_seq = activity_event_seq;
                            activity_event_seq = activity_event_seq.saturating_add(1);
                            if activity_stats.observe_model_request_token_usage(usage, source_seq) {
                                upsert_activity_stats_snapshot(
                                    &canonical_stream,
                                    &mut event_log,
                                    &mut subscribers,
                                    &current_start.agent_id,
                                    activity_stats.snapshot(),
                                )
                                .await;
                            }
                            continue;
                        }
                        BackendEvent::Compaction(
                            crate::backend::BackendCompactionEvent::Progress(progress),
                        ) => {
                            if context_compaction.as_ref().is_some_and(|flight| {
                                flight.operation_id == progress.operation_id
                            }) {
                                let session_id = current_session_id
                                    .as_ref()
                                    .expect("live agent must have session_id");
                                let flight = context_compaction
                                    .as_ref()
                                    .expect("matching compaction flight disappeared");
                                if flight.method == Some(CompactionMethod::InlineFallback)
                                    && matches!(
                                        flight.state,
                                        StoredCompactionState::FallbackPreparing
                                            | StoredCompactionState::FallbackCommitPending
                                    )
                                {
                                    continue;
                                }
                                upsert_context_compaction_snapshot(
                                    &canonical_stream,
                                    &mut event_log,
                                    &mut subscribers,
                                    session_id,
                                    &ContextCompactionNotifyPayload {
                                        operation_id: flight.operation_id.clone(),
                                        agent_id: current_start.agent_id.clone(),
                                        logical_session_id: session_id.clone(),
                                        backend_kind,
                                        trigger: flight.trigger,
                                        method: None,
                                        status: ContextCompactionStatus::Progress {
                                            stage: progress.stage,
                                        },
                                        provider_version: flight.provider_version.clone(),
                                        metrics: CompactionMetrics {
                                            duration_ms: progress.elapsed_ms,
                                            ..CompactionMetrics::default()
                                        },
                                        message: None,
                                    },
                                )
                                .await;
                            }
                            continue;
                        }
                        BackendEvent::Compaction(
                            crate::backend::BackendCompactionEvent::Observed(observed),
                        ) => {
                            let already_correlated = correlated_compaction_observations
                                .iter()
                                .position(|candidate| *candidate == observed.observation_id)
                                .map(|index| correlated_compaction_observations.remove(index))
                                .is_some();
                            let belongs_to_requested_operation = observed.trigger
                                == CompactionTrigger::BackendObservedManual
                                && (already_correlated
                                    || context_compaction.as_ref().is_some_and(|flight| {
                                        matches!(
                                            flight.state,
                                            StoredCompactionState::NativeDispatchPossible
                                                | StoredCompactionState::NativeAccepted
                                        )
                                    }));
                            let post_tokens = observed.metrics.after_tokens;
                            if let Some(session_id) = current_session_id.as_ref() {
                                let session_id = session_id.clone();
                                if let Err(error) = run_session_store_io(
                                    &session_store,
                                    move |store| {
                                        store.update(&session_id, |record| {
                                            record.token_count = post_tokens
                                        })
                                    },
                                )
                                .await
                                {
                                    tracing::warn!(
                                        %error,
                                        "failed to persist observed compaction token count"
                                    );
                                }
                            }
                            let activity_stats_changed =
                                activity_stats.clear_current_context_usage(activity_event_seq);
                            activity_event_seq = activity_event_seq.saturating_add(1);
                            if activity_stats_changed {
                                upsert_activity_stats_snapshot(
                                    &canonical_stream,
                                    &mut event_log,
                                    &mut subscribers,
                                    &current_start.agent_id,
                                    activity_stats.snapshot(),
                                )
                                .await;
                            }
                            if belongs_to_requested_operation {
                                tracing::debug!(
                                    agent_id = %current_start.agent_id,
                                    observation_id = %observed.observation_id.0,
                                    // Absent once the terminal result has taken
                                    // the flight, which is the ordering this
                                    // correlation exists to survive.
                                    operation_id = context_compaction
                                        .as_ref()
                                        .map(|flight| flight.operation_id.0.as_str()),
                                    after_terminal = already_correlated,
                                    "correlated a backend compaction observation with a requested operation"
                                );
                                continue;
                            }
                            let marker = ContextCompactionTimelineEvent {
                                marker_id: observed.observation_id,
                                operation_id: None,
                                trigger: observed.trigger,
                                method: observed.method,
                                backend_kind,
                                provider_session_id: observed.provider_session_id,
                                status: ContextCompactionTimelineStatus::Completed,
                                mutation: CompactionMutation::Completed,
                                metrics: observed.metrics,
                                message: None,
                                timestamp: now_ms(),
                            };
                            if let Some(sequence) = append_compaction_marker_once(
                                &canonical_stream,
                                &mut event_log,
                                &mut subscribers,
                                &mut replay_state,
                                &marker,
                            )
                            .await
                            {
                                persist_compaction_marker(
                                    &canonical_stream,
                                    current_session_id
                                        .as_ref()
                                        .expect("live agent must have session_id"),
                                    sequence,
                                    &marker,
                                )
                                .await;
                            }
                            continue;
                        }
                    };
                    if resume_replay_gate_pending {
                        if !resume_uses_authoritative_transcript {
                            ingest_gated_replay_event(
                                &mut event,
                                &canonical_stream,
                                &current_start.agent_id,
                                &mut event_log,
                                &mut subscribers,
                                &mut replay_state,
                                &mut activity_stats,
                                &mut active_stream_text,
                                &mut activity_event_seq,
                            )
                            .await;
                        }
                        continue;
                    }
                    // Any live backend event is observable turn progress, so it
                    // restarts the supervisor's stall clock. Stream deltas
                    // count and never reach the status watch, which is why the
                    // authoritative clock lives here and not in the scheduler.
                    last_backend_event_at = Some(Instant::now());
                    let mut real_idle_transition = false;
                    let mut synthesize_idle_after_error = false;
                    match &event {
                        ChatEvent::MessageAdded(message) => {
                            if let Some(compaction) = active_compaction.as_mut() {
                                match &message.sender {
                                    MessageSender::Error => {
                                        compaction.error = Some(message.content.clone());
                                    }
                                    MessageSender::Assistant { .. } if compaction.summary.is_empty() => {
                                        push_summary_capped(
                                            &mut compaction.summary,
                                            &message.content,
                                            compaction.max_summary_bytes,
                                        );
                                    }
                                    _ => {}
                                }
                            }
                            if matches!(message.sender, MessageSender::Error) {
                                let diagnostic_mid_turn = in_turn && backend_typing;
                                let error_ends_turn = in_turn
                                    && pending_tool_response_ids.is_empty()
                                    && !backend_typing;
                                if diagnostic_mid_turn {
                                    tracing::info!(
                                        agent_id = %current_start.agent_id,
                                        "backend error event during open turn treated as diagnostic"
                                    );
                                }
                                if error_ends_turn {
                                    tracing::warn!(
                                        agent_id = %current_start.agent_id,
                                        "backend error event ended active turn without idle marker"
                                    );
                                    real_idle_transition = true;
                                    synthesize_idle_after_error = true;
                                    in_turn = false;
                                    idle_transition_armed = false;
                                }
                                let msg = message.content.clone();
                                status_handle.update(|s| {
                                    // A mid-turn diagnostic leaves the turn
                                    // running; reporting it as completed would
                                    // contradict is_thinking.
                                    if !diagnostic_mid_turn {
                                        s.turn_completed = true;
                                    }
                                    if error_ends_turn {
                                        s.is_thinking = false;
                                    }
                                    s.last_error = Some(msg);
                                    s.activity_counter = s.activity_counter.saturating_add(1);
                                }).await;
                            } else {
                                status_handle.update(|s| {
                                    s.activity_counter = s.activity_counter.saturating_add(1);
                                }).await;
                            }
                        }
                        ChatEvent::StreamStart(_) => {
                            active_stream_text.clear();
                            status_handle.update(|s| {
                                s.last_error = None;
                                s.activity_counter = s.activity_counter.saturating_add(1);
                            }).await;
                        }
                        ChatEvent::StreamDelta(delta) => {
                            if let Some(compaction) = active_compaction.as_mut() {
                                push_summary_capped(
                                    &mut compaction.summary,
                                    &delta.text,
                                    compaction.max_summary_bytes,
                                );
                            }
                            active_stream_text.push_str(&delta.text);
                        }
                        ChatEvent::StreamEnd(data) => {
                            if let Some(compaction) = active_compaction.as_mut()
                                && compaction.summary.is_empty()
                            {
                                push_summary_capped(
                                    &mut compaction.summary,
                                    &data.message.content,
                                    compaction.max_summary_bytes,
                                );
                            }
                            active_stream_text.clear();
                            status_handle.update(|s| {
                                s.turn_completed = true;
                                s.last_error = None;
                                s.activity_counter = s.activity_counter.saturating_add(1);
                            }).await;
                        }
                        ChatEvent::TypingStatusChanged(typing) => {
                            let typing = *typing;
                            backend_typing = typing;
                            let mut completed_by_idle = false;
                            if typing {
                                in_turn = true;
                                idle_transition_armed = true;
                            } else if !pending_tool_response_ids.is_empty() {
                                idle_transition_armed = false;
                            } else if in_turn && idle_transition_armed {
                                real_idle_transition = true;
                                completed_by_idle = true;
                                in_turn = false;
                                idle_transition_armed = false;
                            } else if in_turn {
                                tracing::warn!(
                                    agent_id = %current_start.agent_id,
                                    "ignoring backend idle marker before idle was armed"
                                );
                            }
                            let visibly_busy = backend_turn_visibly_busy(
                                typing,
                                pending_tool_response_ids.len(),
                            );
                            status_handle.update(|s| {
                                s.is_thinking = visibly_busy;
                                if completed_by_idle {
                                    s.turn_completed = true;
                                }
                                s.activity_counter = s.activity_counter.saturating_add(1);
                            }).await;
                        }
                        ChatEvent::OperationCancelled(_) => {
                            pending_tool_response_ids.clear();
                            active_agent_await_ids.clear();
                            // Re-arm, rather than ending the turn here. Arming
                            // otherwise happens only on typing-true or on
                            // answering the last pending tool, and a cancelled
                            // interactive tool produces neither — so without
                            // this the backend's own idle marker is discarded as
                            // unarmed, the turn never closes, and every later
                            // message queues behind it with no cancel able to
                            // clear it. Ending the turn *here* instead would
                            // drain that queue before the idle marker the client
                            // is still owed, starting the next turn's response
                            // ahead of the previous turn's end.
                            idle_transition_armed = in_turn;
                            if let Some(compaction) = active_compaction.as_mut() {
                                compaction.error = Some("compaction summary turn was cancelled".to_owned());
                            }
                            status_handle.update(|s| {
                                s.pending_user_response = None;
                                s.is_thinking = false;
                                s.turn_completed = true;
                                s.activity_counter = s.activity_counter.saturating_add(1);
                            }).await;
                        }
                        ChatEvent::ToolRequest(request) => {
                            open_tool_call_ids.insert(request.tool_call_id.clone());
                            open_tool_requests
                                .insert(request.tool_call_id.clone(), request.clone());
                            if matches!(
                                &request.tool_type,
                                protocol::ToolRequestType::TydeAwaitAgents { .. }
                            ) {
                                active_agent_await_ids.insert(request.tool_call_id.clone());
                            }
                            let pending_response_kind = match &request.tool_type {
                                protocol::ToolRequestType::AskUserQuestion { .. } => {
                                    Some(registry::PendingUserResponseKind::UserQuestion)
                                }
                                protocol::ToolRequestType::ExitPlanMode { .. } => {
                                    Some(registry::PendingUserResponseKind::PlanApproval)
                                }
                                _ => None,
                            };
                            if pending_response_kind.is_some() {
                                pending_tool_response_ids.insert(request.tool_call_id.clone());
                                in_turn = true;
                                idle_transition_armed = false;
                            }
                            status_handle.update(|s| {
                                if let Some(pending_response_kind) = pending_response_kind {
                                    s.pending_user_response = Some(pending_response_kind);
                                }
                                s.activity_counter = s.activity_counter.saturating_add(1);
                            }).await;
                        }
                        ChatEvent::ToolExecutionCompleted(completion) => {
                            if completed_tool_call_ids.contains(&completion.tool_call_id) {
                                eprintln!(
                                    "TYDE DUPLICATE TOOL COMPLETION DROPPED agent={} tool_call_id={}",
                                    current_start.agent_id,
                                    completion.tool_call_id,
                                );
                                continue;
                            }
                            completed_tool_call_ids.insert(completion.tool_call_id.clone());
                            open_tool_call_ids.remove(&completion.tool_call_id);
                            open_tool_requests.remove(&completion.tool_call_id);
                            active_agent_await_ids.remove(&completion.tool_call_id);
                            let completed_pending_response =
                                pending_tool_response_ids.remove(&completion.tool_call_id);
                            if completed_pending_response && pending_tool_response_ids.is_empty() && in_turn {
                                idle_transition_armed = true;
                            }
                            let interrupted_tool_ends_turn = !completed_pending_response
                                && in_turn
                                && pending_tool_response_ids.is_empty()
                                && interrupted_tool_completion(completion);
                            if interrupted_tool_ends_turn {
                                tracing::warn!(
                                    agent_id = %current_start.agent_id,
                                    tool_call_id = %completion.tool_call_id,
                                    "interrupted tool completion ended active turn without idle marker"
                                );
                                real_idle_transition = true;
                                synthesize_idle_after_error = true;
                                in_turn = false;
                                idle_transition_armed = false;
                                // This terminal marker stands in for the idle
                                // event the backend never sent; treat the
                                // backend as no longer typing so a later
                                // error-without-idle can still end its turn.
                                backend_typing = false;
                            }
                            status_handle.update(|s| {
                                if completed_pending_response && pending_tool_response_ids.is_empty() {
                                    s.pending_user_response = None;
                                    s.turn_completed = false;
                                    s.is_thinking = true;
                                }
                                if interrupted_tool_ends_turn {
                                    s.turn_completed = true;
                                    s.is_thinking = false;
                                    s.last_error = tool_completion_error(completion);
                                }
                                s.activity_counter = s.activity_counter.saturating_add(1);
                            }).await;
                        }
                        _ => {
                            status_handle.update(|s| {
                                s.activity_counter = s.activity_counter.saturating_add(1);
                            }).await;
                        }
                    }
                    if let Some(update) = apply_runtime_session_updates(
                        &session_store,
                        current_session_id
                            .as_ref()
                            .expect("live agent must have session_id"),
                        &event,
                    )
                    .await
                    {
                        let _ = session_summary_count_tx.send(
                            HostSessionSummaryCountEvent::Update(
                                HostSessionSummaryCountUpdate {
                                    agent_id: agent_id.clone(),
                                    payload: update,
                                },
                            ),
                        );
                    }
                    let source_seq = activity_event_seq;
                    activity_event_seq = activity_event_seq.saturating_add(1);
                    if activity_stats.observe_chat_event(
                        &mut event,
                        source_seq,
                        &active_stream_text,
                    ) {
                        upsert_activity_stats_snapshot(
                            &canonical_stream,
                            &mut event_log,
                            &mut subscribers,
                            &current_start.agent_id,
                            activity_stats.snapshot(),
                        )
                        .await;
                    }
                    append_backend_chat_event(
                        &canonical_stream,
                        &mut event_log,
                        &mut subscribers,
                        &mut replay_state,
                        backend_kind,
                        &events,
                        &event,
                    )
                    .await;
                    if synthesize_idle_after_error {
                        replay_state.discard_active_stream();
                        append_chat_event(
                            &canonical_stream,
                            &mut event_log,
                            &mut subscribers,
                            &mut replay_state,
                            &ChatEvent::TypingStatusChanged(false),
                        )
                        .await;
                    }

                    if real_idle_transition {
                        let session_id = current_session_id
                            .as_ref()
                            .expect("live agent must have session_id");
                        mark_transcript_authoritative(&transcript_store, session_id).await;
                        let capability = backend
                            .as_ref()
                            .expect("backend must exist while actor is running")
                            .compaction_capability();
                        upsert_context_compaction_capability(
                            &canonical_stream,
                            &mut event_log,
                            &mut subscribers,
                            &ContextCompactionCapabilityPayload {
                                agent_id: current_start.agent_id.clone(),
                                logical_session_id: session_id.clone(),
                                availability:
                                    crate::host::requested_compaction_availability(
                                        &capability,
                                        &crate::host::CompactionRoutingPolicy::default(),
                                        transcript_is_authoritative(&transcript_store, session_id)
                                            .await,
                                    ),
                            },
                        )
                        .await;
                    }

                    if real_idle_transition
                        && let Some(compaction) = active_compaction.take()
                    {
                        let session_id = current_session_id
                            .as_ref()
                            .expect("live agent must have session_id");
                        let (reply, result) = complete_compaction(compaction, session_id);
                        if result.is_err() {
                            compaction_blocked = false;
                        }
                        if let Err(error) = &result {
                            status_handle
                                .update(|s| {
                                    s.last_error = Some(error.clone());
                                    s.activity_counter = s.activity_counter.saturating_add(1);
                                })
                                .await;
                        }
                        // Keep normal input blocked after success until the host
                        // either rotates successfully and closes this actor or
                        // explicitly releases it.
                        let _ = reply.send(result);
                    }

                    if real_idle_transition
                        && matches!(lifecycle, ActorLifecycle::Closing)
                    {
                        let reply = close_reply
                            .take()
                            .expect("close requested without pending close reply");
                        let backend = backend
                            .take()
                            .expect("backend must exist while closing a live actor");
                        backend.shutdown().await;
                        abort_resume_replay_barrier_task(&mut resume_replay_barrier_task);
                        terminalize_live_activity(
                            LiveActivityTerminalContext {
                                canonical_stream: &canonical_stream,
                                event_log: &mut event_log,
                                replay_state: &mut replay_state,
                                subscribers: &mut subscribers,
                                open_tool_call_ids: &mut open_tool_call_ids,
                                pending_tool_response_ids: &mut pending_tool_response_ids,
                                active_agent_await_ids: &mut active_agent_await_ids,
                            },
                            LiveActivityTerminalStatus::Stopped,
                            "agent closed",
                        )
                        .await;
                        finish_actor_close(&accepting_input_task, &status_handle, reply).await;
                        return;
                    }

                    if matches!(lifecycle, ActorLifecycle::Running)
                        && context_compaction.is_some()
                    {
                        try_dispatch_context_compaction(
                            ContextCompactionDispatchContext {
                                actor_tx: &actor_tx,
                                backend: backend
                                    .as_ref()
                                    .expect("backend must exist while actor is running")
                                    .as_ref(),
                                session_store: &session_store,
                                transcript_store: &transcript_store,
                                session_id: current_session_id
                                    .as_ref()
                                    .expect("live agent must have session_id"),
                                start: &current_start,
                                status_handle: &status_handle,
                                current_session_settings: &current_session_settings,
                                canonical_stream: &canonical_stream,
                                event_log: &mut event_log,
                                subscribers: &mut subscribers,
                                spawn_config: &compaction_spawn_config,
                                use_mock_backend,
                                capacity_tx: &compaction_capacity_tx,
                                antigravity_conversations_dir:
                                    &compaction_antigravity_conversations_dir,
                            },
                            &mut context_compaction,
                            ContextCompactionDispatchReadiness {
                                queue: &queue,
                                in_turn,
                                replay_pending: resume_replay_gate_pending,
                                open_tool_call_ids: &open_tool_call_ids,
                                pending_tool_response_ids: &pending_tool_response_ids,
                                background_mutation_active: !replay_state
                                    .active_background_progress
                                    .is_empty(),
                            },
                        )
                        .await;
                    }

                    if real_idle_transition
                        && matches!(lifecycle, ActorLifecycle::Running)
                        && !compaction_blocked
                        && !queue.is_empty()
                        && context_compaction.as_ref().is_none_or(|flight| {
                            queue.front().is_some_and(|queued| {
                                flight.admits_queue_sequence(queued.sequence)
                            })
                        })
                    {
                        let queued = queue
                            .pop_front()
                            .expect("queue reported non-empty but pop_front returned None");
                        let review_origin = match queued.origin.as_ref() {
                            Some(MessageOrigin::Review { review_id }) => Some(review_id.clone()),
                            Some(MessageOrigin::User) | Some(MessageOrigin::Supervisor) | None => None,
                        };
                        if let Some(review_id) = review_origin.as_ref() {
                            tracing::info!(
                                review_id = %review_id,
                                agent_id = %current_start.agent_id,
                                session_id = current_session_id
                                    .as_ref()
                                    .map(|id| id.0.as_str())
                                    .unwrap_or("<none>"),
                                queued_message_id = %queued.id,
                                queue_len = queue.len(),
                                message_len = queued.message.len(),
                                images_count = queued.images.len(),
                                "dequeued review-origin bundle"
                            );
                        }
                        update_queued_messages_snapshot(
                            &canonical_stream,
                            &mut event_log,
                            &mut subscribers,
                            &queue,
                            &session_store,
                        )
                        .await;
                        in_turn = true;
                        idle_transition_armed = false;
                        let outcome = backend
                            .as_ref()
                            .expect("backend must exist while actor is running")
                            .send_with_outcome(AgentInput::SendMessage(
                                queued.clone().into_send_payload(),
                            ))
                            .await;
                        match outcome {
                            SendOutcome::Busy(_) => {
                                // The backend opened a turn on its own initiative
                                // before this dispatch landed. Keep the message at
                                // the front of the queue; the self-started turn's
                                // idle marker re-triggers this drain.
                                tracing::info!(
                                    agent_id = %current_start.agent_id,
                                    queued_message_id = %queued.id,
                                    "backend busy with a self-started turn; requeued message at front"
                                );
                                queue.push_front(queued);
                                update_queued_messages_snapshot(
                                    &canonical_stream,
                                    &mut event_log,
                                    &mut subscribers,
                                    &queue,
                                    &session_store,
                                )
                                .await;
                            }
                            SendOutcome::Closed => {
                                if let Some(review_id) = review_origin.as_ref() {
                                    tracing::warn!(
                                        review_id = %review_id,
                                        agent_id = %current_start.agent_id,
                                        queued_message_id = %queued.id,
                                        "failed to send dequeued review-origin bundle to backend"
                                    );
                                }
                                let payload = AgentErrorPayload {
                                    agent_id: current_start.agent_id.clone(),
                                    code: AgentErrorCode::Internal,
                                    message: "agent backend closed".to_owned(),
                                    fatal: true,
                                };
                                terminalize_live_activity(
                                    LiveActivityTerminalContext {
                                        canonical_stream: &canonical_stream,
                                        event_log: &mut event_log,
                                        replay_state: &mut replay_state,
                                        subscribers: &mut subscribers,
                                        open_tool_call_ids: &mut open_tool_call_ids,
                                        pending_tool_response_ids: &mut pending_tool_response_ids,
                                        active_agent_await_ids: &mut active_agent_await_ids,
                                    },
                                    LiveActivityTerminalStatus::Failed,
                                    &payload.message,
                                )
                                .await;
                                enter_terminal_failure(
                                    TerminalFailureContext {
                                        accepting_input: &accepting_input_task,
                                        status_handle: &status_handle,
                                        canonical_stream: &canonical_stream,
                                        event_log: &mut event_log,
                                        replay_state: &mut replay_state,
                                        subscribers: &mut subscribers,
                                        queue: &mut queue,
                                        session_store: &session_store,
                                        compaction: Some(TerminalCompactionFailureContext {
                                            flight: &mut context_compaction,
                                            session_store: &session_store,
                                            session_id: current_session_id
                                                .as_ref()
                                                .expect("live agent must have session_id"),
                                            start: &current_start,
                                            activity_stats: &mut activity_stats,
                                        }),
                                    },
                                    &payload,
                                )
                                .await;
                                park_terminal_agent(
                                    &session_store,
                                    &transcript_store,
                                    current_session_id.as_ref(),
                                    &mut pending_alias,
                                    &mut current_start,
                                    &start_tx,
                                    &mut event_log,
                                    &mut latest_output,
                                    &mut subscribers,
                                    &mut pending_inputs,
                                    &mut rx,
                                )
                                .await;
                                return;
                            }
                            SendOutcome::Accepted => {
                                mark_agent_turn_active(&status_handle).await;
                                if let Some(review_id) = review_origin.as_ref() {
                                    tracing::info!(
                                        review_id = %review_id,
                                        agent_id = %current_start.agent_id,
                                        queued_message_id = %queued.id,
                                        "sent dequeued review-origin bundle to backend"
                                    );
                                }
                                if let Some(review_id) = review_origin {
                                    tracing::debug!(
                                        review_id = %review_id,
                                        agent_id = %current_start.agent_id,
                                        queued_message_id = %queued.id,
                                        "dequeued review-origin bundle sent; notifying consumed"
                                    );
                                    notify_review_bundle_consumed(
                                        &review_registry,
                                        review_id,
                                        &current_start.agent_id,
                                    )
                                    .await;
                                }
                            }
                        }
                    }
                }
                maybe_command = next_agent_command(
                    &mut pending_inputs,
                    &mut rx,
                    !resume_replay_gate_pending,
                ) => {
                    let Some(command) = maybe_command else {
                        break;
                    };
                    // The two input-bearing commands share one delivery path.
                    // Normalizing here keeps that path single-sourced;
                    // `delivery_ack` is the only difference between them, and
                    // the `SendInput` arm below must resolve it on every exit.
                    // An unresolved acknowledgement drops with this iteration,
                    // which the caller reads as a failed delivery.
                    let mut delivery_ack: Option<oneshot::Sender<Result<(), String>>> = None;
                    let command = match command {
                        AgentCommand::DeliverMessage { payload, reply } => {
                            delivery_ack = Some(reply);
                            AgentCommand::SendInput(AgentInput::SendMessage(payload))
                        }
                        command => command,
                    };
                    match command {
                        AgentCommand::ResumeReplayBarrier { result } => {
                            if !resume_replay_gate_pending {
                                continue;
                            }
                            tracing::info!(
                                agent_id = %current_start.agent_id,
                                replay_result = ?result,
                                has_initial_follow_up = initial_follow_up.is_some(),
                                "resume replay barrier settled"
                            );
                            // Drain any replay events already buffered on the
                            // backend stream before closing the gate. The
                            // select! is unbiased, so the barrier command can be
                            // selected while replay events are still queued;
                            // ingesting them here (rather than leaving them for a
                            // now-ungated `events.recv()`) keeps the full resume
                            // transcript off the live broadcast path.
                            while let Ok(event) = events.try_recv_backend() {
                                if resume_uses_authoritative_transcript {
                                    // See the gated receive path above: the
                                    // barrier, not event serialization, is the
                                    // authoritative replay/live boundary.
                                    match event {
                                        event @ BackendEvent::ModelRequestTokenUsage(_) => {
                                            deferred_authoritative_resume_events.push(event);
                                        }
                                        BackendEvent::Chat(_) | BackendEvent::Compaction(_) => {}
                                    }
                                    continue;
                                }
                                match event {
                                    BackendEvent::Chat(mut event) => {
                                        ingest_gated_replay_event(
                                            &mut event,
                                            &canonical_stream,
                                            &current_start.agent_id,
                                            &mut event_log,
                                            &mut subscribers,
                                            &mut replay_state,
                                            &mut activity_stats,
                                            &mut active_stream_text,
                                            &mut activity_event_seq,
                                        )
                                        .await;
                                    }
                                    BackendEvent::ModelRequestTokenUsage(usage) => {
                                        let source_seq = activity_event_seq;
                                        activity_event_seq = activity_event_seq.saturating_add(1);
                                        if activity_stats
                                            .observe_model_request_token_usage(usage, source_seq)
                                        {
                                            upsert_activity_stats_snapshot(
                                                &canonical_stream,
                                                &mut event_log,
                                                &mut subscribers,
                                                &current_start.agent_id,
                                                activity_stats.snapshot(),
                                            )
                                            .await;
                                        }
                                    }
                                    BackendEvent::Compaction(
                                        crate::backend::BackendCompactionEvent::Observed(
                                            observed,
                                        ),
                                    ) => {
                                        let activity_stats_changed = activity_stats
                                            .clear_current_context_usage(activity_event_seq);
                                        activity_event_seq =
                                            activity_event_seq.saturating_add(1);
                                        if activity_stats_changed {
                                            upsert_activity_stats_snapshot(
                                                &canonical_stream,
                                                &mut event_log,
                                                &mut subscribers,
                                                &current_start.agent_id,
                                                activity_stats.snapshot(),
                                            )
                                            .await;
                                        }
                                        let marker =
                                            ContextCompactionTimelineEvent {
                                                marker_id: observed.observation_id,
                                                operation_id: None,
                                                trigger: observed.trigger,
                                                method: observed.method,
                                                backend_kind,
                                                provider_session_id:
                                                    observed.provider_session_id,
                                                status:
                                                    ContextCompactionTimelineStatus::Completed,
                                                mutation:
                                                    CompactionMutation::Completed,
                                                metrics: observed.metrics,
                                                message: None,
                                                timestamp: now_ms(),
                                            };
                                        let _ = append_compaction_marker_once(
                                            &canonical_stream,
                                            &mut event_log,
                                            &mut subscribers,
                                            &mut replay_state,
                                            &marker,
                                        )
                                        .await;
                                    }
                                    BackendEvent::Compaction(
                                        crate::backend::BackendCompactionEvent::Progress(_),
                                    ) => {}
                                }
                            }
                            events.restore_backend_events(
                                deferred_authoritative_resume_events.drain(..),
                            );
                            if result.is_ok()
                                && let Some(task_list) = persisted_resume_task_list.take()
                                && !replay_log_latest_task_snapshot_is(&event_log, &task_list)
                            {
                                let mut event = ChatEvent::TaskUpdate(task_list);
                                ingest_gated_replay_event(
                                    &mut event,
                                    &canonical_stream,
                                    &current_start.agent_id,
                                    &mut event_log,
                                    &mut subscribers,
                                    &mut replay_state,
                                    &mut activity_stats,
                                    &mut active_stream_text,
                                    &mut activity_event_seq,
                                )
                                .await;
                            }
                            resume_replay_gate_pending = false;
                            match result {
                                Ok(()) => {
                                    let session_id = current_session_id
                                        .as_ref()
                                        .expect("live agent must have session_id");
                                    mark_transcript_authoritative(&transcript_store, session_id)
                                        .await;
                                    let capability = backend
                                        .as_ref()
                                        .expect(
                                            "backend must exist after replay barrier",
                                        )
                                        .compaction_capability();
                                    upsert_context_compaction_capability(
                                        &canonical_stream,
                                        &mut event_log,
                                        &mut subscribers,
                                        &ContextCompactionCapabilityPayload {
                                            agent_id:
                                                current_start.agent_id.clone(),
                                            logical_session_id:
                                                session_id.clone(),
                                            availability: crate::host::requested_compaction_availability(
                                                    &capability,
                                                    &crate::host::CompactionRoutingPolicy::default(),
                                                    transcript_is_authoritative(
                                                        &transcript_store,
                                                        session_id,
                                                    )
                                                    .await,
                                                ),
                                        },
                                    )
                                    .await;
                                    accepting_input_task.store(true, Ordering::SeqCst);
                                    // Settling the resumed agent to Idle is only
                                    // honest when nothing is waiting to run. An
                                    // initial follow-up already suppressed it;
                                    // a checked delivery accepted behind this
                                    // gate is the same situation — the caller
                                    // was told the message was accepted, so
                                    // publishing Idle here would let its very
                                    // next wait return before the turn starts.
                                    if initial_follow_up.is_none()
                                        && acknowledged_gated_deliveries == 0
                                        && queue.is_empty()
                                    {
                                        publish_resumed_agent_idle(
                                            &status_handle,
                                            &canonical_stream,
                                            &mut event_log,
                                            &mut subscribers,
                                            &mut replay_state,
                                        )
                                        .await;
                                    }
                                    flush_pending_agent_attaches(
                                        &event_log,
                                        Some(&replay_state),
                                        &mut latest_output,
                                        &mut subscribers,
                                        &mut pending_resume_attaches,
                                        &status_handle,
                                    )
                                    .await;
                                    if initial_follow_up.is_none()
                                        && acknowledged_gated_deliveries == 0
                                        && !queue.is_empty()
                                        && !compaction_blocked
                                        && context_compaction.as_ref().is_none_or(|flight| {
                                            queue.front().is_some_and(|queued| {
                                                flight.admits_queue_sequence(queued.sequence)
                                            })
                                        })
                                    {
                                        let forced_closed =
                                            hold_resume_queue_dispatch_boundary(
                                                &current_start.name,
                                                &mut backend,
                                                &actor_tx,
                                                &mut rx,
                                            )
                                            .await;
                                        let dispatch = if forced_closed {
                                            QueuedMessageDispatchOutcome::Closed
                                        } else {
                                            dispatch_queued_message(
                                                QueuedMessageDispatchContext {
                                                    backend: backend.as_ref().expect(
                                                        "backend must exist after replay barrier",
                                                    ),
                                                queue: &mut queue,
                                                in_turn: &mut in_turn,
                                                idle_transition_armed:
                                                    &mut idle_transition_armed,
                                                canonical_stream: &canonical_stream,
                                                event_log: &mut event_log,
                                                subscribers: &mut subscribers,
                                                agent_id: &current_start.agent_id,
                                                session_store: &session_store,
                                                status_handle: &status_handle,
                                                review_registry: &review_registry,
                                                session_id: current_session_id.as_ref(),
                                                },
                                            )
                                            .await
                                        };
                                        if dispatch == QueuedMessageDispatchOutcome::Closed {
                                            let payload = AgentErrorPayload {
                                                agent_id: current_start.agent_id.clone(),
                                                code: AgentErrorCode::Internal,
                                                message: "agent backend closed".to_owned(),
                                                fatal: true,
                                            };
                                            terminalize_live_activity(
                                                LiveActivityTerminalContext {
                                                    canonical_stream: &canonical_stream,
                                                    event_log: &mut event_log,
                                                    replay_state: &mut replay_state,
                                                    subscribers: &mut subscribers,
                                                    open_tool_call_ids:
                                                        &mut open_tool_call_ids,
                                                    pending_tool_response_ids:
                                                        &mut pending_tool_response_ids,
                                                    active_agent_await_ids:
                                                        &mut active_agent_await_ids,
                                                },
                                                LiveActivityTerminalStatus::Failed,
                                                &payload.message,
                                            )
                                            .await;
                                            enter_terminal_failure(
                                                TerminalFailureContext {
                                                    accepting_input: &accepting_input_task,
                                                    status_handle: &status_handle,
                                                    canonical_stream: &canonical_stream,
                                                    event_log: &mut event_log,
                                                    replay_state: &mut replay_state,
                                                    subscribers: &mut subscribers,
                                                    queue: &mut queue,
                                                    session_store: &session_store,
                                                    compaction: Some(
                                                        TerminalCompactionFailureContext {
                                                            flight: &mut context_compaction,
                                                            session_store: &session_store,
                                                            session_id: current_session_id
                                                                .as_ref()
                                                                .expect(
                                                                    "live agent must have session_id",
                                                                ),
                                                            start: &current_start,
                                                            activity_stats: &mut activity_stats,
                                                        },
                                                    ),
                                                },
                                                &payload,
                                            )
                                            .await;
                                            park_terminal_agent(
                                                &session_store,
                                                &transcript_store,
                                                current_session_id.as_ref(),
                                                &mut pending_alias,
                                                &mut current_start,
                                                &start_tx,
                                                &mut event_log,
                                                &mut latest_output,
                                                &mut subscribers,
                                                &mut pending_inputs,
                                                &mut rx,
                                            )
                                            .await;
                                            return;
                                        }
                                    }
                                    if let Some(input) = initial_follow_up.take()
                                        && !send_initial_follow_up_or_park(
                                            input,
                                            InitialFollowUpContext {
                                                backend: &mut backend,
                                                in_turn: &mut in_turn,
                                                idle_transition_armed: &mut idle_transition_armed,
                                                session_store: &session_store,
                                                transcript_store: &transcript_store,
                                                current_session_id: current_session_id.as_ref(),
                                                pending_alias: &mut pending_alias,
                                                current_start: &mut current_start,
                                                start_tx: &start_tx,
                                                accepting_input: &accepting_input_task,
                                                status_handle: &status_handle,
                                                canonical_stream: &canonical_stream,
                                                event_log: &mut event_log,
                                                latest_output: &mut latest_output,
                                                replay_state: &mut replay_state,
                                                subscribers: &mut subscribers,
                                                queue: &mut queue,
                                                next_queue_sequence:
                                                    &mut next_queue_sequence,
                                                pending_inputs: &mut pending_inputs,
                                                rx: &mut rx,
                                            },
                                        )
                                        .await
                                    {
                                        abort_resume_replay_barrier_task(
                                            &mut resume_replay_barrier_task,
                                        );
                                        return;
                                    }
                                }
                                Err(err) => {
                                    accepting_input_task.store(false, Ordering::SeqCst);
                                    let payload = AgentErrorPayload {
                                        agent_id: current_start.agent_id.clone(),
                                        code: AgentErrorCode::BackendFailed,
                                        message: format!(
                                            "failed to resume agent history before live replay boundary: {err}"
                                        ),
                                        fatal: true,
                                    };
                                    terminalize_live_activity(
                                        LiveActivityTerminalContext {
                                            canonical_stream: &canonical_stream,
                                            event_log: &mut event_log,
                                            replay_state: &mut replay_state,
                                            subscribers: &mut subscribers,
                                            open_tool_call_ids: &mut open_tool_call_ids,
                                            pending_tool_response_ids: &mut pending_tool_response_ids,
                                            active_agent_await_ids: &mut active_agent_await_ids,
                                        },
                                        LiveActivityTerminalStatus::Failed,
                                        &payload.message,
                                    )
                                    .await;
                                    enter_terminal_failure(
                                        TerminalFailureContext {
                                            accepting_input: &accepting_input_task,
                                            status_handle: &status_handle,
                                            canonical_stream: &canonical_stream,
                                            event_log: &mut event_log,
                                            replay_state: &mut replay_state,
                                            subscribers: &mut subscribers,
                                            queue: &mut queue,
                                            session_store: &session_store,
                                            compaction: Some(TerminalCompactionFailureContext {
                                                flight: &mut context_compaction,
                                                session_store: &session_store,
                                                session_id: current_session_id
                                                    .as_ref()
                                                    .expect("live agent must have session_id"),
                                                start: &current_start,
                                                activity_stats: &mut activity_stats,
                                            }),
                                        },
                                        &payload,
                                    )
                                    .await;
                                    flush_pending_agent_attaches(
                                        &event_log,
                                        None,
                                        &mut latest_output,
                                        &mut subscribers,
                                        &mut pending_resume_attaches,
                                        &status_handle,
                                    )
                                    .await;
                                    if let Some(backend) = backend.take() {
                                        backend.shutdown()
                                        .await;
                                    }
                                    park_terminal_agent(
                                        &session_store,
                                        &transcript_store,
                                        current_session_id.as_ref(),
                                        &mut pending_alias,
                                        &mut current_start,
                                        &start_tx,
                                        &mut event_log,
                                        &mut latest_output,
                                        &mut subscribers,
                                        &mut pending_inputs,
                                        &mut rx,
                                    )
                                    .await;
                                    return;
                                }
                            }
                        }
                        AgentCommand::DeliverMessage { reply, .. } => {
                            // Normalization above rewrites every delivery into
                            // `SendInput`, so this is unreachable. Fail closed
                            // rather than assert: a caller must never read an
                            // unhandled command as a delivered message.
                            let _ = reply.send(Err(DELIVERY_NOT_ACKNOWLEDGED.to_owned()));
                        }
                        AgentCommand::SendInput(input) => {
                            if resume_replay_gate_pending {
                                // Accepted, not rejected: the resume barrier is
                                // transient and the message is dispatched once
                                // it lifts. Mark the queued turn active before
                                // acknowledging so the caller cannot read Idle,
                                // and record the acceptance so completing the
                                // barrier cannot publish Idle back over it.
                                if delivery_ack.is_some() {
                                    mark_agent_turn_active(&status_handle).await;
                                    acknowledged_gated_deliveries =
                                        acknowledged_gated_deliveries.saturating_add(1);
                                }
                                pending_inputs.push_back(input);
                                if let Some(reply) = delivery_ack.take() {
                                    let _ = reply.send(Ok(()));
                                }
                                continue;
                            }
                            if matches!(lifecycle, ActorLifecycle::Closing) {
                                // An acknowledged delivery has a caller to
                                // answer. Fire-and-forget input has a human
                                // watching the chat instead, so it needs a
                                // transcript rejection — dropping it silently
                                // left a typed message with no trace at all.
                                if !reject_agent_delivery(
                                    delivery_ack.take(),
                                    DELIVERY_REJECTED_CLOSING,
                                ) {
                                    let payload = closing_input_rejected_payload(
                                        &current_start.agent_id,
                                    );
                                    append_event(
                                        &canonical_stream,
                                        &mut event_log,
                                        &mut subscribers,
                                        FrameKind::AgentError,
                                        &payload,
                                    )
                                    .await;
                                }
                                continue;
                            }
                            if active_compaction.is_some() || compaction_blocked {
                                if reject_agent_delivery(
                                    delivery_ack.take(),
                                    DELIVERY_REJECTED_COMPACTING,
                                ) {
                                    continue;
                                }
                                let payload =
                                    compaction_input_rejected_payload(&current_start.agent_id);
                                append_event(
                                    &canonical_stream,
                                    &mut event_log,
                                    &mut subscribers,
                                    FrameKind::AgentError,
                                    &payload,
                                )
                                .await;
                                continue;
                            }
                            match input {
                                AgentInput::SendMessage(msg) => {
                                    let admitted_tool_response = msg.tool_response.clone();
                                    let admitted_message = msg.message.clone();
                                    let admitted_images = msg.images.clone();
                                    let review_origin = match msg.origin.as_ref() {
                                        Some(MessageOrigin::Review { review_id }) => {
                                            Some(review_id.clone())
                                        }
                                        Some(MessageOrigin::User) | Some(MessageOrigin::Supervisor) | None => None,
                                    };
                                    let message_len = msg.message.len();
                                    let images_count = msg.images.as_ref().map_or(0, Vec::len);
                                    let review_origin_for_queue = match msg.origin.clone() {
                                        Some(MessageOrigin::Review { review_id }) => Some(review_id),
                                        Some(MessageOrigin::User) | Some(MessageOrigin::Supervisor) | None => None,
                                    };
                                    let stale_tool_response = matches!(
                                        msg.tool_response.as_ref(),
                                        Some(
                                            protocol::SendMessageToolResponse::ExitPlanMode {
                                                tool_call_id,
                                                ..
                                            }
                                            | protocol::SendMessageToolResponse::AskUserQuestion {
                                                tool_call_id,
                                                ..
                                            }
                                        ) if !pending_tool_response_ids.contains(tool_call_id)
                                    );
                                    if stale_tool_response {
                                        if !reject_agent_delivery(
                                            delivery_ack.take(),
                                            "tool response does not match a pending interaction",
                                        ) {
                                            let event = stale_tool_response_rejected_event();
                                            append_chat_event(
                                                &canonical_stream,
                                                &mut event_log,
                                                &mut subscribers,
                                                &mut replay_state,
                                                &event,
                                            )
                                            .await;
                                        }
                                        continue;
                                    }
                                    let invalid_tool_response = msg
                                        .tool_response
                                        .as_ref()
                                        .and_then(|response| {
                                            let tool_call_id = match response {
                                                protocol::SendMessageToolResponse::ExitPlanMode {
                                                    tool_call_id,
                                                    ..
                                                }
                                                | protocol::SendMessageToolResponse::AskUserQuestion {
                                                    tool_call_id,
                                                    ..
                                                } => tool_call_id,
                                            };
                                            let request = open_tool_requests.get(tool_call_id)?;
                                            match (response, &request.tool_type) {
                                                (
                                                    protocol::SendMessageToolResponse::AskUserQuestion {
                                                        answer,
                                                        ..
                                                    },
                                                    protocol::ToolRequestType::AskUserQuestion { .. },
                                                ) if answer.trim().is_empty() => Some(
                                                    "A question response must contain an answer",
                                                ),
                                                (
                                                    protocol::SendMessageToolResponse::ExitPlanMode {
                                                        decision: protocol::ExitPlanModeDecision::Approve,
                                                        feedback: Some(_),
                                                        ..
                                                    },
                                                    protocol::ToolRequestType::ExitPlanMode { .. },
                                                ) => Some(
                                                    "Plan approval cannot include rejection feedback",
                                                ),
                                                (
                                                    protocol::SendMessageToolResponse::AskUserQuestion { .. },
                                                    protocol::ToolRequestType::AskUserQuestion { .. },
                                                )
                                                | (
                                                    protocol::SendMessageToolResponse::ExitPlanMode { .. },
                                                    protocol::ToolRequestType::ExitPlanMode { .. },
                                                ) => None,
                                                _ => Some(
                                                    "Tool response kind does not match the pending request",
                                                ),
                                            }
                                        });
                                    if let Some(message) = invalid_tool_response {
                                        if !reject_agent_delivery(
                                            delivery_ack.take(),
                                            message,
                                        ) {
                                            let event = tool_response_rejected_event(message);
                                            append_chat_event(
                                                &canonical_stream,
                                                &mut event_log,
                                                &mut subscribers,
                                                &mut replay_state,
                                                &event,
                                            )
                                            .await;
                                        }
                                        continue;
                                    }
                                    let is_tool_response = msg.tool_response.is_some();
                                    let clear_pending_response = match msg.tool_response.as_ref() {
                                        Some(protocol::SendMessageToolResponse::ExitPlanMode {
                                            tool_call_id,
                                            ..
                                        }
                                        | protocol::SendMessageToolResponse::AskUserQuestion {
                                            tool_call_id,
                                            ..
                                        }) if pending_tool_response_ids.contains(tool_call_id) => {
                                            Some(pending_tool_response_ids.len() == 1)
                                        }
                                        _ => None,
                                    };
                                    if !is_tool_response {
                                        status_handle
                                            .update(|status| {
                                                status.activity_counter =
                                                    status.activity_counter.saturating_add(1);
                                            })
                                            .await;
                                    }
                                    if (in_turn || context_compaction.is_some())
                                        && !is_tool_response
                                    {
                                        let queued_message_id =
                                            QueuedMessageId(Uuid::new_v4().to_string());
                                        let sequence = next_queue_sequence;
                                        next_queue_sequence =
                                            next_queue_sequence.saturating_add(1);
                                        queue.push_back(SequencedQueuedMessage {
                                            sequence,
                                            entry: QueuedMessageEntry {
                                                id: queued_message_id.clone(),
                                                message: msg.message,
                                                images: msg.images.unwrap_or_default(),
                                                origin: msg.origin,
                                            },
                                        });
                                        if let Some(review_id) = review_origin_for_queue {
                                            tracing::info!(
                                                review_id = %review_id,
                                                agent_id = %current_start.agent_id,
                                                session_id = current_session_id
                                                    .as_ref()
                                                    .map(|id| id.0.as_str())
                                                    .unwrap_or("<none>"),
                                                queued_message_id = %queued_message_id,
                                                queue_len = queue.len(),
                                                message_len,
                                                images_count,
                                                "queued review-origin bundle"
                                            );
                                        }
                                        update_queued_messages_snapshot(
                                            &canonical_stream,
                                            &mut event_log,
                                            &mut subscribers,
                                            &queue,
                                            &session_store,
                                        )
                                        .await;
                                        if let Some(reply) = delivery_ack.take() {
                                            // Queued behind the open turn counts
                                            // as accepted. Mark active first:
                                            // between a cancellation and the
                                            // backend's idle marker the status
                                            // reads completed while `in_turn` is
                                            // still true, so acknowledging
                                            // without this would leave the agent
                                            // Idle with a message it never ran.
                                            mark_agent_turn_active(&status_handle).await;
                                            let _ = reply.send(Ok(()));
                                        }
                                    } else {
                                        if !is_tool_response {
                                            in_turn = true;
                                            idle_transition_armed = false;
                                        }
                                        if let Some(review_id) = review_origin.as_ref() {
                                            tracing::info!(
                                                review_id = %review_id,
                                                agent_id = %current_start.agent_id,
                                                session_id = current_session_id
                                                    .as_ref()
                                                    .map(|id| id.0.as_str())
                                                    .unwrap_or("<none>"),
                                                queue_len = queue.len(),
                                                message_len,
                                                images_count,
                                                "sending review-origin bundle to backend"
                                            );
                                        }
                                        let backend_ref = backend
                                            .as_ref()
                                            .expect("backend must exist while actor is running");
                                        let outcome = backend_ref
                                            .send_with_outcome(AgentInput::SendMessage(msg))
                                            .await;
                                        if let SendOutcome::Busy(input) = outcome {
                                            match input {
                                                AgentInput::SendMessage(payload)
                                                    if payload.tool_response.is_none() =>
                                                {
                                                    tracing::info!(
                                                        agent_id = %current_start.agent_id,
                                                        "backend busy with a self-started turn; queued message at front"
                                                    );
                                                    let sequence = next_queue_sequence;
                                                    next_queue_sequence =
                                                        next_queue_sequence.saturating_add(1);
                                                    queue.push_front(SequencedQueuedMessage {
                                                        sequence,
                                                        entry: queued_entry_from_send_payload(
                                                            payload,
                                                        ),
                                                    });
                                                    update_queued_messages_snapshot(
                                                        &canonical_stream,
                                                        &mut event_log,
                                                        &mut subscribers,
                                                        &queue,
                                                        &session_store,
                                                    )
                                                    .await;
                                                    if let Some(reply) = delivery_ack.take() {
                                                        // A Busy hand-back is a
                                                        // requeue, not a refusal:
                                                        // the actor still owns
                                                        // the message and will
                                                        // send it when the
                                                        // self-started turn ends.
                                                        mark_agent_turn_active(&status_handle)
                                                            .await;
                                                        let _ = reply.send(Ok(()));
                                                    }
                                                }
                                                _ => {
                                                    // Tool responses answer the
                                                    // backend's active turn, so a
                                                    // busy hand-back for one is a
                                                    // backend contract violation.
                                                    tracing::error!(
                                                        agent_id = %current_start.agent_id,
                                                        "backend handed back a non-requeueable input as Busy"
                                                    );
                                                    reject_agent_delivery(
                                                        delivery_ack.take(),
                                                        DELIVERY_NOT_ACKNOWLEDGED,
                                                    );
                                                }
                                            }
                                            // The requeued (or rejected) input was
                                            // not delivered: skip the post-send
                                            // bookkeeping below so e.g. a review
                                            // bundle is not marked consumed.
                                            continue;
                                        } else if matches!(outcome, SendOutcome::Closed) {
                                            if let Some(review_id) = review_origin.as_ref() {
                                                tracing::warn!(
                                                    review_id = %review_id,
                                                    agent_id = %current_start.agent_id,
                                                    session_id = current_session_id
                                                        .as_ref()
                                                        .map(|id| id.0.as_str())
                                                        .unwrap_or("<none>"),
                                                    "failed to send review-origin bundle to backend"
                                                );
                                            }
                                            // Report the refusal before the
                                            // terminal transition so the caller
                                            // learns its message was never
                                            // delivered, not merely that the
                                            // agent later died.
                                            reject_agent_delivery(
                                                delivery_ack.take(),
                                                DELIVERY_REJECTED_BACKEND_CLOSED,
                                            );
                                            let payload = AgentErrorPayload {
                                                agent_id: current_start.agent_id.clone(),
                                                code: AgentErrorCode::Internal,
                                                message: "agent backend closed".to_owned(),
                                                fatal: true,
                                            };
                                            terminalize_live_activity(
                                                LiveActivityTerminalContext {
                                                    canonical_stream: &canonical_stream,
                                                    event_log: &mut event_log,
                                                    replay_state: &mut replay_state,
                                                    subscribers: &mut subscribers,
                                                    open_tool_call_ids: &mut open_tool_call_ids,
                                                    pending_tool_response_ids: &mut pending_tool_response_ids,
                                                    active_agent_await_ids: &mut active_agent_await_ids,
                                                },
                                                LiveActivityTerminalStatus::Failed,
                                                &payload.message,
                                            )
                                            .await;
                                            enter_terminal_failure(
                                                TerminalFailureContext {
                                                    accepting_input: &accepting_input_task,
                                                    status_handle: &status_handle,
                                                    canonical_stream: &canonical_stream,
                                                    event_log: &mut event_log,
                                                    replay_state: &mut replay_state,
                                                    subscribers: &mut subscribers,
                                                    queue: &mut queue,
                                                    session_store: &session_store,
                                                    compaction: Some(TerminalCompactionFailureContext {
                                                        flight: &mut context_compaction,
                                                        session_store: &session_store,
                                                        session_id: current_session_id
                                                            .as_ref()
                                                            .expect("live agent must have session_id"),
                                                        start: &current_start,
                                                        activity_stats: &mut activity_stats,
                                                    }),
                                                },
                                                &payload,
                                            )
                                            .await;
                                            park_terminal_agent(
                                                &session_store,
                                                &transcript_store,
                                                current_session_id.as_ref(),
                                                &mut pending_alias,
                                                &mut current_start,
                                                &start_tx,
                                                &mut event_log,
                                                &mut latest_output,
                                                &mut subscribers,
                                                &mut pending_inputs,
                                                &mut rx,
                                            )
                                            .await;
                                            return;
                                        }
                                        if !is_tool_response {
                                            mark_agent_turn_active(&status_handle).await;
                                        }
                                        if let Some(tool_response) = admitted_tool_response {
                                            let tool_call_id = match &tool_response {
                                                protocol::SendMessageToolResponse::AskUserQuestion {
                                                    tool_call_id,
                                                    ..
                                                }
                                                | protocol::SendMessageToolResponse::ExitPlanMode {
                                                    tool_call_id,
                                                    ..
                                                } => tool_call_id.clone(),
                                            };
                                            let request = open_tool_requests
                                                .remove(&tool_call_id)
                                                .unwrap_or_else(|| {
                                                    panic!(
                                                        "admitted tool response lost request {tool_call_id}"
                                                    )
                                                });
                                            let result = match (&tool_response, &request.tool_type) {
                                                (
                                                    protocol::SendMessageToolResponse::AskUserQuestion {
                                                        answer,
                                                        ..
                                                    },
                                                    protocol::ToolRequestType::AskUserQuestion { .. },
                                                ) => serde_json::json!({ "answer": answer }),
                                                (
                                                    protocol::SendMessageToolResponse::ExitPlanMode {
                                                        decision,
                                                        feedback,
                                                        ..
                                                    },
                                                    protocol::ToolRequestType::ExitPlanMode {
                                                        plan,
                                                        plan_path,
                                                    },
                                                ) => {
                                                    let mut result = serde_json::Map::new();
                                                    result.insert(
                                                        "decision".to_owned(),
                                                        serde_json::Value::String(
                                                            match decision {
                                                                protocol::ExitPlanModeDecision::Approve => "approved",
                                                                protocol::ExitPlanModeDecision::Reject => "rejected",
                                                            }
                                                            .to_owned(),
                                                        ),
                                                    );
                                                    if let Some(feedback) = feedback {
                                                        result.insert(
                                                            "feedback".to_owned(),
                                                            serde_json::Value::String(feedback.clone()),
                                                        );
                                                    }
                                                    if let Some(plan) = plan {
                                                        result.insert(
                                                            "plan".to_owned(),
                                                            serde_json::Value::String(plan.clone()),
                                                        );
                                                    }
                                                    if let Some(plan_path) = plan_path {
                                                        result.insert(
                                                            "plan_path".to_owned(),
                                                            serde_json::Value::String(plan_path.clone()),
                                                        );
                                                    }
                                                    serde_json::Value::Object(result)
                                                }
                                                _ => panic!(
                                                    "tool response kind did not match request {request:?}"
                                                ),
                                            };
                                            append_chat_event(
                                                &canonical_stream,
                                                &mut event_log,
                                                &mut subscribers,
                                                &mut replay_state,
                                                &ChatEvent::MessageAdded(ChatMessage {
                                                    message_id: None,
                                                    timestamp: now_ms(),
                                                    sender: MessageSender::User,
                                                    content: admitted_message,
                                                    reasoning: None,
                                                    tool_calls: Vec::new(),
                                                    model_info: None,
                                                    token_usage: None,
                                                    context_breakdown: None,
                                                    images: admitted_images,
                                                }),
                                            )
                                            .await;
                                            append_chat_event(
                                                &canonical_stream,
                                                &mut event_log,
                                                &mut subscribers,
                                                &mut replay_state,
                                                &ChatEvent::ToolExecutionCompleted(
                                                    ToolExecutionCompletedData {
                                                        tool_call_id: tool_call_id.clone(),
                                                        outcome: ToolExecutionOutcome::Succeeded {
                                                            result: ToolExecutionResult::Other {
                                                                result,
                                                            },
                                                        },
                                                    },
                                                ),
                                            )
                                            .await;
                                            completed_tool_call_ids.insert(tool_call_id.clone());
                                            mark_transcript_authoritative(
                                                &transcript_store,
                                                current_session_id.as_ref().expect(
                                                    "live agent must have session_id",
                                                ),
                                            )
                                            .await;
                                            eprintln!(
                                                "TYDE TOOL RESPONSE COMMIT session={} tool_call_id={} event_log_len={}",
                                                current_session_id
                                                    .as_ref()
                                                    .expect("live agent must have session_id")
                                                    .0,
                                                tool_call_id,
                                                event_log.len(),
                                            );
                                            open_tool_call_ids.remove(&tool_call_id);
                                            let completed_pending_response =
                                                pending_tool_response_ids.remove(&tool_call_id);
                                            if completed_pending_response
                                                && pending_tool_response_ids.is_empty()
                                                && in_turn
                                            {
                                                idle_transition_armed = true;
                                            }
                                        }
                                        if let Some(clear_pending_response) = clear_pending_response {
                                            status_handle
                                                .update(|s| {
                                                    if clear_pending_response {
                                                        s.pending_user_response = None;
                                                    }
                                                    s.turn_completed = false;
                                                    s.is_thinking = true;
                                                    s.activity_counter =
                                                        s.activity_counter.saturating_add(1);
                                                })
                                                .await;
                                        }
                                        if let Some(reply) = delivery_ack.take() {
                                            if is_tool_response {
                                                // Agent-control delivery carries
                                                // a plain follow-up, never a tool
                                                // response, but the active-before-
                                                // acknowledge contract has to hold
                                                // for whatever reaches here.
                                                mark_agent_turn_active(&status_handle).await;
                                            }
                                            let _ = reply.send(Ok(()));
                                        }
                                        if let Some(review_id) = review_origin {
                                            tracing::debug!(
                                                review_id = %review_id,
                                                agent_id = %current_start.agent_id,
                                                "review-origin bundle sent; notifying consumed"
                                            );
                                            notify_review_bundle_consumed(
                                                &review_registry,
                                                review_id,
                                                &current_start.agent_id,
                                            )
                                            .await;
                                        }
                                    }
                                }
                                AgentInput::EditQueuedMessage(payload) => {
                                    let Some(entry) =
                                        queue.iter_mut().find(|entry| entry.id == payload.id)
                                    else {
                                        emit_unknown_queued_message_error(
                                            &canonical_stream,
                                            &mut event_log,
                                            &mut subscribers,
                                            &current_start.agent_id,
                                            &payload.id,
                                        )
                                        .await;
                                        continue;
                                    };
                                    entry.message = payload.message;
                                    entry.images = payload.images;
                                    update_queued_messages_snapshot(
                                        &canonical_stream,
                                        &mut event_log,
                                        &mut subscribers,
                                        &queue,
                                        &session_store,
                                    )
                                    .await;
                                }
                                AgentInput::CancelQueuedMessage(payload) => {
                                    let Some(index) =
                                        queue.iter().position(|entry| entry.id == payload.id)
                                    else {
                                        emit_unknown_queued_message_error(
                                            &canonical_stream,
                                            &mut event_log,
                                            &mut subscribers,
                                            &current_start.agent_id,
                                            &payload.id,
                                        )
                                        .await;
                                        continue;
                                    };
                                    let removed = queue.remove(index);
                                    assert!(
                                        removed.is_some(),
                                        "queue remove failed for index {index} after position()"
                                    );
                                    update_queued_messages_snapshot(
                                        &canonical_stream,
                                        &mut event_log,
                                        &mut subscribers,
                                        &queue,
                                        &session_store,
                                    )
                                    .await;
                                }
                                AgentInput::SendQueuedMessageNow(payload) => {
                                    let Some(index) =
                                        queue.iter().position(|entry| entry.id == payload.id)
                                    else {
                                        emit_unknown_queued_message_error(
                                            &canonical_stream,
                                            &mut event_log,
                                            &mut subscribers,
                                            &current_start.agent_id,
                                            &payload.id,
                                        )
                                        .await;
                                        continue;
                                    };
                                    let queued = queue
                                        .remove(index)
                                        .expect("queue remove failed after position()");
                                    if let Some(MessageOrigin::Review { review_id }) =
                                        queued.origin.as_ref()
                                    {
                                        tracing::info!(
                                            review_id = %review_id,
                                            agent_id = %current_start.agent_id,
                                            session_id = current_session_id
                                                .as_ref()
                                                .map(|id| id.0.as_str())
                                                .unwrap_or("<none>"),
                                            queued_message_id = %queued.id,
                                            queue_len = queue.len(),
                                            message_len = queued.message.len(),
                                            images_count = queued.images.len(),
                                            "moved review-origin bundle to front of queue"
                                        );
                                    }
                                    queue.push_front(queued);
                                    update_queued_messages_snapshot(
                                        &canonical_stream,
                                        &mut event_log,
                                        &mut subscribers,
                                        &queue,
                                        &session_store,
                                    )
                                    .await;

                                    if in_turn {
                                        if !backend
                                            .as_ref()
                                            .expect("backend must exist while actor is running")
                                            .interrupt()
                                            .await
                                        {
                                            let payload = AgentErrorPayload {
                                                agent_id: current_start.agent_id.clone(),
                                                code: AgentErrorCode::Internal,
                                                message: "agent backend does not support interrupt"
                                                    .to_owned(),
                                                fatal: false,
                                            };
                                            append_event(
                                                &canonical_stream,
                                                &mut event_log,
                                                &mut subscribers,
                                                FrameKind::AgentError,
                                                &payload,
                                            )
                                            .await;
                                        }
                                        continue;
                                    }

                                    let queued = queue
                                        .pop_front()
                                        .expect("queue front must exist after push_front");
                                    let review_origin = match queued.origin.as_ref() {
                                        Some(MessageOrigin::Review { review_id }) => {
                                            Some(review_id.clone())
                                        }
                                        Some(MessageOrigin::User) | Some(MessageOrigin::Supervisor) | None => None,
                                    };
                                    if let Some(review_id) = review_origin.as_ref() {
                                        tracing::info!(
                                            review_id = %review_id,
                                            agent_id = %current_start.agent_id,
                                            session_id = current_session_id
                                                .as_ref()
                                                .map(|id| id.0.as_str())
                                                .unwrap_or("<none>"),
                                            queued_message_id = %queued.id,
                                            queue_len = queue.len(),
                                            message_len = queued.message.len(),
                                            images_count = queued.images.len(),
                                            "dequeued review-origin bundle for immediate send"
                                        );
                                    }
                                    update_queued_messages_snapshot(
                                        &canonical_stream,
                                        &mut event_log,
                                        &mut subscribers,
                                        &queue,
                                        &session_store,
                                    )
                                    .await;
                                    in_turn = true;
                                    idle_transition_armed = false;
                                    let outcome = backend
                                        .as_ref()
                                        .expect("backend must exist while actor is running")
                                        .send_with_outcome(AgentInput::SendMessage(
                                            queued.clone().into_send_payload(),
                                        ))
                                        .await;
                                    match outcome {
                                        SendOutcome::Busy(_) => {
                                            tracing::info!(
                                                agent_id = %current_start.agent_id,
                                                queued_message_id = %queued.id,
                                                "backend busy with a self-started turn; send-now message requeued at front"
                                            );
                                            queue.push_front(queued);
                                            update_queued_messages_snapshot(
                                                &canonical_stream,
                                                &mut event_log,
                                                &mut subscribers,
                                                &queue,
                                                &session_store,
                                            )
                                            .await;
                                        }
                                        SendOutcome::Closed => {
                                            if let Some(review_id) = review_origin.as_ref() {
                                                tracing::warn!(
                                                    review_id = %review_id,
                                                    agent_id = %current_start.agent_id,
                                                    queued_message_id = %queued.id,
                                                    "failed to send immediate review-origin bundle to backend"
                                                );
                                            }
                                            let payload = AgentErrorPayload {
                                                agent_id: current_start.agent_id.clone(),
                                                code: AgentErrorCode::Internal,
                                                message: "agent backend closed".to_owned(),
                                                fatal: true,
                                            };
                                            terminalize_live_activity(
                                                LiveActivityTerminalContext {
                                                    canonical_stream: &canonical_stream,
                                                    event_log: &mut event_log,
                                                    replay_state: &mut replay_state,
                                                    subscribers: &mut subscribers,
                                                    open_tool_call_ids: &mut open_tool_call_ids,
                                                    pending_tool_response_ids: &mut pending_tool_response_ids,
                                                    active_agent_await_ids: &mut active_agent_await_ids,
                                                },
                                                LiveActivityTerminalStatus::Failed,
                                                &payload.message,
                                            )
                                            .await;
                                            enter_terminal_failure(
                                                TerminalFailureContext {
                                                    accepting_input: &accepting_input_task,
                                                    status_handle: &status_handle,
                                                    canonical_stream: &canonical_stream,
                                                    event_log: &mut event_log,
                                                    replay_state: &mut replay_state,
                                                    subscribers: &mut subscribers,
                                                    queue: &mut queue,
                                                    session_store: &session_store,
                                                    compaction: Some(TerminalCompactionFailureContext {
                                                        flight: &mut context_compaction,
                                                        session_store: &session_store,
                                                        session_id: current_session_id
                                                            .as_ref()
                                                            .expect("live agent must have session_id"),
                                                        start: &current_start,
                                                        activity_stats: &mut activity_stats,
                                                    }),
                                                },
                                                &payload,
                                            )
                                            .await;
                                            park_terminal_agent(
                                                &session_store,
                                                &transcript_store,
                                                current_session_id.as_ref(),
                                                &mut pending_alias,
                                                &mut current_start,
                                                &start_tx,
                                                &mut event_log,
                                                &mut latest_output,
                                                &mut subscribers,
                                                &mut pending_inputs,
                                                &mut rx,
                                            )
                                            .await;
                                            return;
                                        }
                                        SendOutcome::Accepted => {
                                            if let Some(review_id) = review_origin.as_ref() {
                                                tracing::info!(
                                                    review_id = %review_id,
                                                    agent_id = %current_start.agent_id,
                                                    queued_message_id = %queued.id,
                                                    "sent immediate review-origin bundle to backend"
                                                );
                                            }
                                            if let Some(review_id) = review_origin {
                                                tracing::debug!(
                                                    review_id = %review_id,
                                                    agent_id = %current_start.agent_id,
                                                    queued_message_id = %queued.id,
                                                    "immediate review-origin bundle sent; notifying consumed"
                                                );
                                                notify_review_bundle_consumed(
                                                    &review_registry,
                                                    review_id,
                                                    &current_start.agent_id,
                                                )
                                                .await;
                                            }
                                        }
                                    }
                                }
                                AgentInput::UpdateSessionSettings(update) => {
                                    let Some(session_schema) = session_schema.as_ref() else {
                                        let payload = AgentErrorPayload {
                                            agent_id: current_start.agent_id.clone(),
                                            code: AgentErrorCode::Internal,
                                            message: "session settings schema unavailable".to_owned(),
                                            fatal: false,
                                        };
                                        append_event(
                                            &canonical_stream,
                                            &mut event_log,
                                            &mut subscribers,
                                            FrameKind::AgentError,
                                            &payload,
                                        )
                                        .await;
                                        continue;
                                    };
                                    let mut updated_session_settings =
                                        current_session_settings.clone();
                                    apply_session_settings_update(
                                        &mut updated_session_settings,
                                        &update.values,
                                    );
                                    if let Err(err) = validate_session_settings_values(
                                        session_schema,
                                        &updated_session_settings,
                                    ) {
                                        let payload = AgentErrorPayload {
                                            agent_id: current_start.agent_id.clone(),
                                            code: AgentErrorCode::Internal,
                                            message: err,
                                            fatal: false,
                                        };
                                        append_event(
                                            &canonical_stream,
                                            &mut event_log,
                                            &mut subscribers,
                                            FrameKind::AgentError,
                                            &payload,
                                        )
                                        .await;
                                        append_event(
                                            &canonical_stream,
                                            &mut event_log,
                                            &mut subscribers,
                                            FrameKind::SessionSettings,
                                            &SessionSettingsPayload {
                                                values: current_session_settings.clone(),
                                            },
                                        )
                                        .await;
                                        continue;
                                    }
                                    if let Err(err) = validate_runtime_session_settings_update(
                                        current_start.backend_kind,
                                        &current_session_settings,
                                        &update.values,
                                    ) {
                                        let payload = AgentErrorPayload {
                                            agent_id: current_start.agent_id.clone(),
                                            code: AgentErrorCode::Internal,
                                            message: err,
                                            fatal: false,
                                        };
                                        append_event(
                                            &canonical_stream,
                                            &mut event_log,
                                            &mut subscribers,
                                            FrameKind::AgentError,
                                            &payload,
                                        )
                                        .await;
                                        append_event(
                                            &canonical_stream,
                                            &mut event_log,
                                            &mut subscribers,
                                            FrameKind::SessionSettings,
                                            &SessionSettingsPayload {
                                                values: current_session_settings.clone(),
                                            },
                                        )
                                        .await;
                                        continue;
                                    }
                                    let mut backend_update = update.clone();
                                    if current_start.backend_kind == BackendKind::Hermes {
                                        backend_update
                                            .values
                                            .0
                                            .remove(crate::backend::hermes::HERMES_PROFILE_SETTING);
                                    }
                                    if let Err(err) = backend
                                        .as_mut()
                                        .expect("backend must exist while actor is running")
                                        .update_session_settings(backend_update)
                                        .await
                                    {
                                        let payload = AgentErrorPayload {
                                            agent_id: current_start.agent_id.clone(),
                                            code: AgentErrorCode::BackendFailed,
                                            message: format!(
                                                "failed to apply session settings: {err}"
                                            ),
                                            fatal: false,
                                        };
                                        append_event(
                                            &canonical_stream,
                                            &mut event_log,
                                            &mut subscribers,
                                            FrameKind::AgentError,
                                            &payload,
                                        )
                                        .await;
                                        append_event(
                                            &canonical_stream,
                                            &mut event_log,
                                            &mut subscribers,
                                            FrameKind::SessionSettings,
                                            &SessionSettingsPayload {
                                                values: current_session_settings.clone(),
                                            },
                                        )
                                        .await;
                                        continue;
                                    }
                                    current_session_settings = updated_session_settings;
                                    if let Err(err) = session_store
                                        .lock()
                                        .await
                                        .set_session_settings(
                                            current_session_id
                                                .as_ref()
                                                .expect("live agent must have session_id"),
                                            current_session_settings.clone(),
                                        )
                                    {
                                        tracing::error!(
                                            "failed to persist session settings for {}: {}",
                                            current_session_id
                                                .as_ref()
                                                .expect("live agent must have session_id"),
                                            err
                                        );
                                    }
                                    append_event(
                                        &canonical_stream,
                                        &mut event_log,
                                        &mut subscribers,
                                        FrameKind::SessionSettings,
                                        &SessionSettingsPayload {
                                            values: current_session_settings.clone(),
                                        },
                                    )
                                    .await;
                                }
                            }
                        }
                        AgentCommand::ReadCompactionCapability { reply } => {
                            let capability = backend
                                .as_ref()
                                .expect("backend must exist while actor is running")
                                .compaction_capability();
                            let _ = reply.send(capability);
                        }
                        AgentCommand::ReadRequestedCompactionRoute { trigger, reply } => {
                            let result = match current_session_id.as_ref() {
                                Some(session_id) => {
                                    let capability = backend
                                        .as_ref()
                                        .expect("backend must exist while actor is running")
                                        .compaction_capability();
                                    crate::host::requested_context_compaction_route(
                                        &capability,
                                        trigger,
                                        transcript_is_authoritative(
                                            &transcript_store,
                                            session_id,
                                        )
                                        .await,
                                    )
                                }
                                None => Err(
                                    "agent has no logical session to compact".to_owned(),
                                ),
                            };
                            let _ = reply.send(result);
                        }
                        AgentCommand::RequestContextCompaction {
                            trigger,
                            focus,
                            barrier_timeout,
                            inactivity_gate,
                            reply,
                        } => {
                            let Some(session_id) = current_session_id.as_ref() else {
                                let _ = reply.send(Err(
                                    "agent has no logical session to compact".to_owned(),
                                ));
                                continue;
                            };
                            if matches!(lifecycle, ActorLifecycle::Closing) {
                                let _ = reply.send(Err("agent is closing".to_owned()));
                                continue;
                            }
                            if inactivity_gate.is_some() {
                                wait_for_compact_if_inactive_test_gate(
                                    &current_start.agent_id,
                                )
                                .await;
                            }
                            if let Some((
                                expected_activity_counter,
                                expected_supervisor_settings_epoch,
                                supervisor_settings_rx,
                            )) = inactivity_gate
                            {
                                let live_activity_counter =
                                    status_handle.snapshot().await.activity_counter;
                                let live_settings = *supervisor_settings_rx.borrow();
                                if live_activity_counter != expected_activity_counter
                                    || live_settings.epoch
                                        != expected_supervisor_settings_epoch
                                    || in_turn
                                    || !queue.is_empty()
                                {
                                    let _ = reply.send(Err(
                                        "supervisor compaction admission became stale"
                                            .to_owned(),
                                    ));
                                    continue;
                                }
                            }
                            if active_compaction.is_some()
                                || compaction_blocked
                            {
                                let _ = reply.send(Err(
                                    "agent compaction is already in progress".to_owned(),
                                ));
                                continue;
                            }
                            if let Some(existing) = context_compaction.as_ref() {
                                if trigger
                                    == CompactionTrigger::SupervisorRequested
                                {
                                    let _ = reply.send(Ok(
                                        existing.operation_id.clone(),
                                    ));
                                } else {
                                    let _ = reply.send(Err(
                                        "agent compaction is already in progress"
                                            .to_owned(),
                                    ));
                                }
                                continue;
                            }
                            let capability = backend
                                .as_ref()
                                .expect("backend must exist while actor is running")
                                .compaction_capability();
                            if let Err(error) = crate::host::requested_context_compaction_route(
                                &capability,
                                trigger,
                                transcript_is_authoritative(
                                    &transcript_store,
                                    session_id,
                                )
                                .await,
                            ) {
                                let _ = reply.send(Err(error));
                                continue;
                            }
                            let operation_id =
                                CompactionOperationId(Uuid::new_v4().to_string());
                            let session_id_for_read = session_id.clone();
                            let session_snapshot = run_session_store_io(
                                &session_store,
                                move |store| {
                                    Ok(store
                                        .get(&session_id_for_read)
                                        .map(|record| record.active_backend_binding_generation))
                                },
                            )
                            .await;
                            let binding_generation = match session_snapshot {
                                Ok(Some(snapshot)) => snapshot,
                                Ok(None) => {
                                    let _ = reply.send(Err(
                                        "agent session metadata is missing".to_owned(),
                                    ));
                                    continue;
                                }
                                Err(error) => {
                                    let _ = reply.send(Err(format!(
                                        "failed to read agent session metadata: {error}"
                                    )));
                                    continue;
                                }
                            };
                            let transcript_high_water = event_log.len() as u64;
                            let operation = CompactionOperationRecord {
                                operation_id: operation_id.clone(),
                                logical_session_id: session_id.clone(),
                                trigger,
                                state: StoredCompactionState::Deferred,
                                method: None,
                                accepted: false,
                                mutation: CompactionMutation::NotObserved,
                                binding_generation_before: binding_generation,
                                binding_generation_after: None,
                                transcript_high_water,
                                metrics: CompactionMetrics::default(),
                                message: None,
                                started_at_ms: now_ms(),
                                finished_at_ms: None,
                            };
                            let session_id_for_write = session_id.clone();
                            if let Err(error) = run_session_store_io(
                                &session_store,
                                move |store| {
                                    store.put_compaction_operation(
                                        &session_id_for_write,
                                        operation,
                                    )
                                },
                            )
                            .await
                            {
                                let _ = reply.send(Err(format!(
                                    "failed to persist compaction operation: {error}"
                                )));
                                continue;
                            }
                            let queue_watermark = next_queue_sequence.saturating_sub(1);
                            context_compaction = Some(CompactionFlight {
                                operation_id: operation_id.clone(),
                                trigger,
                                focus,
                                queue_watermark,
                                state: StoredCompactionState::Deferred,
                                binding_generation_before: binding_generation,
                                fallback_transcript_high_water: None,
                                fallback_activity_counter: None,
                                fallback_settings: None,
                                fallback_task: None,
                                retry_armed: false,
                                retry_attempt: 0,
                                method: None,
                                provider_version: capability.provider_version.clone(),
                                terminal_taken: false,
                            });
                            upsert_context_compaction_snapshot(
                                &canonical_stream,
                                &mut event_log,
                                &mut subscribers,
                                session_id,
                                &ContextCompactionNotifyPayload {
                                    operation_id: operation_id.clone(),
                                    agent_id: current_start.agent_id.clone(),
                                    logical_session_id: session_id.clone(),
                                    backend_kind,
                                    trigger,
                                    method: None,
                                    status: ContextCompactionStatus::Deferred {
                                        stage: CompactionStage::WaitingForIdle,
                                    },
                                    provider_version: capability.provider_version,
                                    metrics: CompactionMetrics::default(),
                                    message: None,
                                },
                            )
                            .await;
                            let deadline_operation_id = operation_id.clone();
                            let deadline_tx = actor_tx.clone();
                            tokio::spawn(async move {
                                tokio::time::sleep(barrier_timeout).await;
                                let _ = deadline_tx.send(
                                    AgentCommand::ContextCompactionBarrierExpired {
                                        operation_id: deadline_operation_id,
                                    },
                                );
                            });
                            let _ = reply.send(Ok(operation_id));
                            try_dispatch_context_compaction(
                                ContextCompactionDispatchContext {
                                    actor_tx: &actor_tx,
                                    backend: backend
                                        .as_ref()
                                        .expect("backend must exist while actor is running")
                                        .as_ref(),
                                    session_store: &session_store,
                                    transcript_store: &transcript_store,
                                    session_id,
                                    start: &current_start,
                                    status_handle: &status_handle,
                                    current_session_settings: &current_session_settings,
                                    canonical_stream: &canonical_stream,
                                    event_log: &mut event_log,
                                    subscribers: &mut subscribers,
                                    spawn_config: &compaction_spawn_config,
                                    use_mock_backend,
                                    capacity_tx: &compaction_capacity_tx,
                                    antigravity_conversations_dir:
                                        &compaction_antigravity_conversations_dir,
                                },
                                &mut context_compaction,
                                ContextCompactionDispatchReadiness {
                                    queue: &queue,
                                    in_turn,
                                    replay_pending: resume_replay_gate_pending,
                                    open_tool_call_ids: &open_tool_call_ids,
                                    pending_tool_response_ids: &pending_tool_response_ids,
                                    background_mutation_active: !replay_state
                                        .active_background_progress
                                        .is_empty(),
                                },
                            )
                            .await;
                        }
                        AgentCommand::ContextCompactionBarrierExpired { operation_id } => {
                            if context_compaction.as_ref().is_some_and(|flight| {
                                flight.operation_id == operation_id
                                    && matches!(
                                        flight.state,
                                        StoredCompactionState::Deferred
                                            | StoredCompactionState::FallbackPreparing
                                            | StoredCompactionState::FallbackCommitPending
                                    )
                            }) {
                                let _ = actor_tx.send(
                                    AgentCommand::ContextCompactionTerminal {
                                        operation_id,
                                        result: Err(
                                            "compaction did not reach a safe point before its barrier deadline"
                                                .to_owned(),
                                        ),
                                    },
                                );
                            }
                        }
                        AgentCommand::RetryContextCompaction { operation_id } => {
                            let Some(active) = context_compaction.as_mut() else {
                                continue;
                            };
                            if active.operation_id != operation_id
                                || active.state != StoredCompactionState::Deferred
                            {
                                continue;
                            }
                            active.retry_armed = false;
                            try_dispatch_context_compaction(
                                ContextCompactionDispatchContext {
                                    actor_tx: &actor_tx,
                                    backend: backend
                                        .as_ref()
                                        .expect("backend must exist while actor is running")
                                        .as_ref(),
                                    session_store: &session_store,
                                    transcript_store: &transcript_store,
                                    session_id: current_session_id
                                        .as_ref()
                                        .expect("live agent must have session_id"),
                                    start: &current_start,
                                    status_handle: &status_handle,
                                    current_session_settings: &current_session_settings,
                                    canonical_stream: &canonical_stream,
                                    event_log: &mut event_log,
                                    subscribers: &mut subscribers,
                                    spawn_config: &compaction_spawn_config,
                                    use_mock_backend,
                                    capacity_tx: &compaction_capacity_tx,
                                    antigravity_conversations_dir:
                                        &compaction_antigravity_conversations_dir,
                                },
                                &mut context_compaction,
                                ContextCompactionDispatchReadiness {
                                    queue: &queue,
                                    in_turn,
                                    replay_pending: resume_replay_gate_pending,
                                    open_tool_call_ids: &open_tool_call_ids,
                                    pending_tool_response_ids: &pending_tool_response_ids,
                                    background_mutation_active: !replay_state
                                        .active_background_progress
                                        .is_empty(),
                                },
                            )
                            .await;
                            if let Some(active) = context_compaction.as_mut().filter(|flight| {
                                flight.operation_id == operation_id
                                    && flight.state == StoredCompactionState::Deferred
                            }) {
                                arm_context_compaction_retry(active, &actor_tx);
                            }
                        }
                        AgentCommand::ContextCompactionFallbackPrepared {
                            operation_id,
                            result,
                        } => {
                            let Some(active) = context_compaction.as_mut() else {
                                if let Ok(prepared) = result {
                                    prepared.binding.backend.shutdown().await;
                                }
                                continue;
                            };
                            if active.operation_id != operation_id
                                || active.state != StoredCompactionState::FallbackPreparing
                            {
                                if let Ok(prepared) = result {
                                    prepared.binding.backend.shutdown().await;
                                }
                                continue;
                            }
                            active.fallback_task.take();
                            let live_activity_counter =
                                status_handle.snapshot().await.activity_counter;
                            let prepared = match result {
                                Ok(prepared) => prepared,
                                Err(error) => {
                                    let _ = actor_tx.send(
                                        AgentCommand::ContextCompactionTerminal {
                                            operation_id,
                                            result: Err(format!(
                                                "inline fallback preparation failed: {error}"
                                            )),
                                        },
                                    );
                                    continue;
                                }
                            };
                            let activity_matches = active
                                .fallback_activity_counter
                                .is_some_and(|counter| counter == live_activity_counter);
                            let transcript_matches = active
                                .fallback_transcript_high_water
                                .is_some_and(|high_water| {
                                    high_water == event_log.len() as u64
                                });
                            let settings_match = active
                                .fallback_settings
                                .as_ref()
                                .is_some_and(|settings| {
                                    settings == &current_session_settings
                                });
                            let revalidation_forced = false;
                            if !activity_matches
                                || !transcript_matches
                                || !settings_match
                                || revalidation_forced
                            {
                                prepared.binding.backend.shutdown().await;
                                let _ = actor_tx.send(
                                    AgentCommand::ContextCompactionTerminal {
                                        operation_id,
                                        result: Err(
                                            "inline fallback commit revalidation rejected changed actor state"
                                                .to_owned(),
                                        ),
                                    },
                                );
                                continue;
                            }
                            active.state = StoredCompactionState::FallbackCommitPending;
                            let session_id = current_session_id
                                .as_ref()
                                .expect("live agent must have session_id");
                            let session_id_for_frontier = session_id.clone();
                            let operation_id_for_frontier = operation_id.clone();
                            if let Err(error) = run_session_store_io(
                                &session_store,
                                move |store| {
                                    let mut record = store
                                        .compaction_operation(
                                            &session_id_for_frontier,
                                            &operation_id_for_frontier,
                                        )
                                        .ok_or_else(|| {
                                            format!(
                                                "missing compaction operation {} before fallback commit",
                                                operation_id_for_frontier.0
                                            )
                                        })?;
                                    record.state =
                                        StoredCompactionState::FallbackCommitPending;
                                    store.put_compaction_operation(
                                        &session_id_for_frontier,
                                        record,
                                    )
                                },
                            )
                            .await
                            {
                                prepared.binding.backend.shutdown().await;
                                let _ = actor_tx.send(
                                    AgentCommand::ContextCompactionTerminal {
                                        operation_id,
                                        result: Err(format!(
                                            "failed to persist fallback commit frontier: {error}"
                                        )),
                                    },
                                );
                                continue;
                            }
                            let crate::backend::PreparedBackendBinding {
                                backend: prepared_handle,
                                events: prepared_events,
                                provider_session_id,
                                ..
                            } = prepared.binding;
                            let prepared_backend =
                                match prepare_backend_handle_for_adoption(
                                    prepared_handle,
                                    &current_start,
                                    &compaction_sub_agent_context,
                                )
                                .await
                                {
                                    Ok(backend) => backend,
                                    Err(error) => {
                                        let _ = actor_tx.send(
                                            AgentCommand::ContextCompactionTerminal {
                                                operation_id,
                                                result: Err(error),
                                            },
                                        );
                                        continue;
                                    }
                                };
                            let expected_generation =
                                active.binding_generation_before;
                            let session_id_for_commit = session_id.clone();
                            let operation_id_for_commit = operation_id.clone();
                            let commit_metrics = prepared.metrics.clone();
                            let provider_session_id_for_commit =
                                provider_session_id.clone();
                            if let Err(error) = run_session_store_io(
                                &session_store,
                                move |store| {
                                    store
                                        .commit_compacted_binding(
                                            &session_id_for_commit,
                                            CommitCompactedBinding {
                                                operation_id: operation_id_for_commit,
                                                expected_generation,
                                                backend_kind,
                                                provider_session_id:
                                                    provider_session_id_for_commit,
                                                metrics: commit_metrics,
                                                message: None,
                                            },
                                        )
                                        .map(|_| ())
                                },
                            )
                            .await
                            {
                                prepared_backend.shutdown()
                                .await;
                                let _ = actor_tx.send(
                                    AgentCommand::ContextCompactionTerminal {
                                        operation_id,
                                        result: Err(format!(
                                            "failed to commit prepared fallback binding: {error}"
                                        )),
                                    },
                                );
                                continue;
                            }
                            let old_backend = backend
                                .replace(prepared_backend)
                                .expect("live actor must have old backend during fallback");
                            events = prepared_events;
                            let adopted_capability = backend
                                .as_ref()
                                .expect("prepared backend must be adopted")
                                .compaction_capability();
                            upsert_context_compaction_capability(
                                &canonical_stream,
                                &mut event_log,
                                &mut subscribers,
                                &ContextCompactionCapabilityPayload {
                                    agent_id: current_start.agent_id.clone(),
                                    logical_session_id: session_id.clone(),
                                    availability:
                                        crate::host::requested_compaction_availability(
                                            &adopted_capability,
                                            &crate::host::CompactionRoutingPolicy::default(),
                                            transcript_is_authoritative(
                                                &transcript_store,
                                                session_id,
                                            )
                                            .await,
                                        ),
                                },
                            )
                            .await;
                            let flight = context_compaction
                                .take()
                                .expect("fallback flight disappeared after durable commit");
                            record_context_compaction_terminal(
                                flight,
                                ContextCompactionTerminalRecord {
                                    accepted: false,
                                    mutation: CompactionMutation::Completed,
                                    method: Some(CompactionMethod::InlineFallback),
                                    metrics: prepared.metrics,
                                    provider_session_id: Some(provider_session_id),
                                    status:
                                        ContextCompactionTimelineStatus::Completed,
                                    message: None,
                                    trusted_post_context_tokens: Some(None),
                                },
                                &session_store,
                                session_id,
                                &current_start,
                                &canonical_stream,
                                &mut event_log,
                                &mut replay_state,
                                &mut subscribers,
                                &mut activity_stats,
                                Some(&mut activity_event_seq),
                            )
                            .await;
                            old_backend.shutdown()
                            .await;
                            let dispatch = release_context_compaction_barrier(
                                backend
                                    .as_ref()
                                    .expect("prepared backend must be adopted"),
                                &mut queue,
                                &mut in_turn,
                                &mut idle_transition_armed,
                                &canonical_stream,
                                &mut event_log,
                                &mut subscribers,
                                &current_start.agent_id,
                                &session_store,
                                &status_handle,
                                &review_registry,
                                current_session_id.as_ref(),
                            )
                            .await;
                            if dispatch == QueuedMessageDispatchOutcome::Closed {
                                terminalize_closed_queue_dispatch(
                                    QueueDispatchTerminalContext {
                                        accepting_input: &accepting_input_task,
                                        status_handle: &status_handle,
                                        canonical_stream: &canonical_stream,
                                        event_log: &mut event_log,
                                        replay_state: &mut replay_state,
                                        subscribers: &mut subscribers,
                                        queue: &mut queue,
                                        session_store: &session_store,
                                        transcript_store: &transcript_store,
                                        context_compaction: &mut context_compaction,
                                        activity_stats: &mut activity_stats,
                                        current_session_id: current_session_id.as_ref(),
                                        pending_alias: &mut pending_alias,
                                        current_start: &mut current_start,
                                        start_tx: &start_tx,
                                        latest_output: &mut latest_output,
                                        pending_inputs: &mut pending_inputs,
                                        rx: &mut rx,
                                        open_tool_call_ids: &mut open_tool_call_ids,
                                        pending_tool_response_ids: &mut pending_tool_response_ids,
                                        active_agent_await_ids: &mut active_agent_await_ids,
                                    },
                                )
                                .await;
                                return;
                            }
                        }
                        AgentCommand::ContextCompactionTerminal {
                            operation_id,
                            result,
                        } => {
                            let Some(mut flight) = context_compaction.take() else {
                                continue;
                            };
                            if flight.operation_id != operation_id {
                                context_compaction = Some(flight);
                                continue;
                            }
                            // Taking the flight above is what breaks the
                            // correlation the observation handler relies on, so
                            // name the observation before that matters.
                            if let Ok(result) = result.as_ref()
                                && let Some(observation_id) = result.evidence.observation_id()
                            {
                                correlated_compaction_observations.push_back(observation_id);
                                while correlated_compaction_observations.len() > 8 {
                                    correlated_compaction_observations.pop_front();
                                }
                            }
                            let session_id = current_session_id
                                .as_ref()
                                .expect("live agent must have session_id");
                            if inline_fallback_owns_structured_native_terminal(
                                &flight,
                                &operation_id,
                                &result,
                            ) {
                                context_compaction = Some(flight);
                                continue;
                            }
                            let rejected_without_mutation = result
                                .as_ref()
                                .ok()
                                .is_some_and(|result| {
                                    backend_compaction_result_allows_inline_fallback(
                                        &operation_id,
                                        result,
                                    )
                                });
                            if rejected_without_mutation
                                && compaction_flight_can_enter_rejected_fallback(&flight)
                            {
                                let rejection_message = result
                                    .as_ref()
                                    .ok()
                                    .and_then(|result| result.outcome.as_ref().err())
                                    .map(|failure| failure.message.clone())
                                    .unwrap_or_else(|| {
                                        "native compaction was rejected before mutation".to_owned()
                                    });
                                let capability = backend
                                    .as_ref()
                                    .expect("backend must exist while starting fallback")
                                    .compaction_capability();
                                let mut fallback_context = ContextCompactionDispatchContext {
                                    actor_tx: &actor_tx,
                                    backend: backend
                                        .as_ref()
                                        .expect("backend must exist while starting fallback")
                                        .as_ref(),
                                    session_store: &session_store,
                                    transcript_store: &transcript_store,
                                    session_id,
                                    start: &current_start,
                                    status_handle: &status_handle,
                                    current_session_settings: &current_session_settings,
                                    canonical_stream: &canonical_stream,
                                    event_log: &mut event_log,
                                    subscribers: &mut subscribers,
                                    spawn_config: &compaction_spawn_config,
                                    use_mock_backend,
                                    capacity_tx: &compaction_capacity_tx,
                                    antigravity_conversations_dir:
                                        &compaction_antigravity_conversations_dir,
                                };
                                let fallback_result = begin_inline_context_fallback(
                                    &mut fallback_context,
                                    &mut flight,
                                    &capability,
                                    format!(
                                        "native compaction was rejected before mutation ({rejection_message}); preparing inline fallback"
                                    ),
                                )
                                .await;
                                in_turn = false;
                                idle_transition_armed = false;
                                match fallback_result {
                                    Ok(()) => {
                                        context_compaction = Some(flight);
                                        continue;
                                    }
                                    Err(error) => {
                                        let terminal = ContextCompactionTerminalRecord {
                                            accepted: false,
                                            mutation: CompactionMutation::NotObserved,
                                            method: flight.method,
                                            metrics: CompactionMetrics::default(),
                                            provider_session_id: None,
                                            status: ContextCompactionTimelineStatus::Failed,
                                            message: Some(error),
                                            trusted_post_context_tokens: None,
                                        };
                                        record_context_compaction_terminal(
                                            flight,
                                            terminal,
                                            &session_store,
                                            session_id,
                                            &current_start,
                                            &canonical_stream,
                                            &mut event_log,
                                            &mut replay_state,
                                            &mut subscribers,
                                            &mut activity_stats,
                                            Some(&mut activity_event_seq),
                                        )
                                        .await;
                                        if matches!(lifecycle, ActorLifecycle::Running) {
                                            let dispatch = release_context_compaction_barrier(
                                                backend.as_ref().expect(
                                                    "backend must exist while releasing compaction",
                                                ),
                                                &mut queue,
                                                &mut in_turn,
                                                &mut idle_transition_armed,
                                                &canonical_stream,
                                                &mut event_log,
                                                &mut subscribers,
                                                &current_start.agent_id,
                                                &session_store,
                                                &status_handle,
                                                &review_registry,
                                                current_session_id.as_ref(),
                                            )
                                            .await;
                                            if dispatch
                                                == QueuedMessageDispatchOutcome::Closed
                                            {
                                                terminalize_closed_queue_dispatch(
                                                    QueueDispatchTerminalContext {
                                                        accepting_input: &accepting_input_task,
                                                        status_handle: &status_handle,
                                                        canonical_stream: &canonical_stream,
                                                        event_log: &mut event_log,
                                                        replay_state: &mut replay_state,
                                                        subscribers: &mut subscribers,
                                                        queue: &mut queue,
                                                        session_store: &session_store,
                                                        transcript_store: &transcript_store,
                                                        context_compaction:
                                                            &mut context_compaction,
                                                        activity_stats: &mut activity_stats,
                                                        current_session_id:
                                                            current_session_id.as_ref(),
                                                        pending_alias: &mut pending_alias,
                                                        current_start: &mut current_start,
                                                        start_tx: &start_tx,
                                                        latest_output: &mut latest_output,
                                                        pending_inputs: &mut pending_inputs,
                                                        rx: &mut rx,
                                                        open_tool_call_ids:
                                                            &mut open_tool_call_ids,
                                                        pending_tool_response_ids:
                                                            &mut pending_tool_response_ids,
                                                        active_agent_await_ids:
                                                            &mut active_agent_await_ids,
                                                    },
                                                )
                                                .await;
                                                return;
                                            }
                                        }
                                        continue;
                                    }
                                }
                            }
                            let accepted_before_terminal = matches!(
                                flight.state,
                                StoredCompactionState::NativeAccepted
                            ) || flight.terminal_taken;
                            let terminal = match result {
                                Ok(result) => {
                                    if result.operation_id != operation_id {
                                        ContextCompactionTerminalRecord {
                                            accepted: accepted_before_terminal,
                                            mutation: if accepted_before_terminal {
                                                CompactionMutation::MayHaveMutated
                                            } else {
                                                CompactionMutation::NotObserved
                                            },
                                            method: flight.method,
                                            metrics: CompactionMetrics::default(),
                                            provider_session_id: None,
                                            status: ContextCompactionTimelineStatus::Failed,
                                            message: Some(format!(
                                                "backend returned terminal result for operation {}",
                                                result.operation_id.0
                                            )),
                                            trusted_post_context_tokens:
                                                accepted_before_terminal.then_some(None),
                                        }
                                    } else {
                                    let mutation: CompactionMutation =
                                        result.mutation.into();
                                    let mut metrics = result.metrics;
                                    let method = result
                                        .outcome
                                        .as_ref()
                                        .ok()
                                        .map(|success| success.mechanism);
                                    let accepted = matches!(
                                        result.dispatch,
                                        crate::backend::BackendCompactionDispatchState::Accepted
                                            | crate::backend::BackendCompactionDispatchState::MayHaveReachedProvider
                                    );
                                    let (status, message) = match result.outcome {
                                        Ok(_) if mutation == CompactionMutation::Completed => {
                                            (ContextCompactionTimelineStatus::Completed, None)
                                        }
                                        Ok(_) => (
                                            ContextCompactionTimelineStatus::Failed,
                                            Some(
                                                "backend reported success without a confirmed compaction boundary"
                                                    .to_owned(),
                                            ),
                                        ),
                                        Err(failure) => (
                                            ContextCompactionTimelineStatus::Failed,
                                            Some(failure.message),
                                        ),
                                    };
                                    let trusted_post_context_tokens = if matches!(
                                        mutation,
                                        CompactionMutation::Completed
                                            | CompactionMutation::MayHaveMutated
                                    ) {
                                        match result.post_context_tokens {
                                            crate::backend::PostCompactionTokenCount::Trusted(
                                                tokens,
                                            ) => {
                                                metrics.after_tokens.get_or_insert(tokens);
                                                Some(Some(tokens))
                                            }
                                            crate::backend::PostCompactionTokenCount::Unknown => {
                                                metrics.after_tokens = None;
                                                Some(None)
                                            }
                                        }
                                    } else {
                                        None
                                    };
                                    ContextCompactionTerminalRecord {
                                        accepted,
                                        mutation,
                                        method,
                                        metrics,
                                        provider_session_id: result.provider_session_id,
                                        status,
                                        message,
                                        trusted_post_context_tokens,
                                    }
                                    }
                                }
                                Err(error) => ContextCompactionTerminalRecord {
                                    accepted: accepted_before_terminal,
                                    mutation: if accepted_before_terminal {
                                        CompactionMutation::MayHaveMutated
                                    } else {
                                        CompactionMutation::NotObserved
                                    },
                                    method: flight.method,
                                    metrics: CompactionMetrics::default(),
                                    provider_session_id: None,
                                    status: ContextCompactionTimelineStatus::Failed,
                                    message: Some(error),
                                    trusted_post_context_tokens:
                                        accepted_before_terminal.then_some(None),
                                },
                            };
                            record_context_compaction_terminal(
                                flight,
                                terminal,
                                &session_store,
                                session_id,
                                &current_start,
                                &canonical_stream,
                                &mut event_log,
                                &mut replay_state,
                                &mut subscribers,
                                &mut activity_stats,
                                Some(&mut activity_event_seq),
                            )
                            .await;
                            if accepted_before_terminal {
                                in_turn = false;
                                idle_transition_armed = false;
                            }
                            if matches!(lifecycle, ActorLifecycle::Running) {
                                let dispatch = release_context_compaction_barrier(
                                    backend
                                        .as_ref()
                                        .expect("backend must exist while releasing compaction"),
                                    &mut queue,
                                    &mut in_turn,
                                    &mut idle_transition_armed,
                                    &canonical_stream,
                                    &mut event_log,
                                    &mut subscribers,
                                    &current_start.agent_id,
                                    &session_store,
                                    &status_handle,
                                    &review_registry,
                                    current_session_id.as_ref(),
                                )
                                .await;
                                if dispatch == QueuedMessageDispatchOutcome::Closed {
                                    terminalize_closed_queue_dispatch(
                                        QueueDispatchTerminalContext {
                                            accepting_input: &accepting_input_task,
                                            status_handle: &status_handle,
                                            canonical_stream: &canonical_stream,
                                            event_log: &mut event_log,
                                            replay_state: &mut replay_state,
                                            subscribers: &mut subscribers,
                                            queue: &mut queue,
                                            session_store: &session_store,
                                            transcript_store: &transcript_store,
                                            context_compaction: &mut context_compaction,
                                            activity_stats: &mut activity_stats,
                                            current_session_id: current_session_id.as_ref(),
                                            pending_alias: &mut pending_alias,
                                            current_start: &mut current_start,
                                            start_tx: &start_tx,
                                            latest_output: &mut latest_output,
                                            pending_inputs: &mut pending_inputs,
                                            rx: &mut rx,
                                            open_tool_call_ids: &mut open_tool_call_ids,
                                            pending_tool_response_ids:
                                                &mut pending_tool_response_ids,
                                            active_agent_await_ids: &mut active_agent_await_ids,
                                        },
                                    )
                                    .await;
                                    return;
                                }
                            }
                        }
                        AgentCommand::CompactIfInactive {
                            expected_activity_counter,
                            expected_supervisor_settings_epoch,
                            supervisor_settings_rx,
                            summary_prompt,
                            max_summary_bytes,
                            accepted,
                            reply,
                        } => {
                            wait_for_compact_if_inactive_test_gate(&current_start.agent_id).await;
                            let live_activity_counter =
                                status_handle.snapshot().await.activity_counter;
                            let live_settings = *supervisor_settings_rx.borrow();
                            let reject = if live_settings.epoch
                                != expected_supervisor_settings_epoch
                            {
                                Some(format!(
                                    "supervisor settings changed before automatic compaction (expected epoch {expected_supervisor_settings_epoch}, current {})",
                                    live_settings.epoch
                                ))
                            } else if live_activity_counter != expected_activity_counter {
                                Some(format!(
                                    "agent activity changed before automatic compaction (expected {expected_activity_counter}, current {live_activity_counter})"
                                ))
                            } else if matches!(lifecycle, ActorLifecycle::Closing) {
                                Some("agent is closing".to_owned())
                            } else if current_start.origin == AgentOrigin::BackendNative {
                                Some("backend-native agents cannot be compacted".to_owned())
                            } else if active_compaction.is_some() || compaction_blocked {
                                Some("agent compaction is already in progress".to_owned())
                            } else if current_session_id.is_none() {
                                Some("agent has no session to compact".to_owned())
                            } else if in_turn {
                                Some("agent is busy".to_owned())
                            } else if !queue.is_empty() {
                                Some("agent has queued work".to_owned())
                            } else {
                                None
                            };
                            if let Some(error) = reject {
                                let _ = accepted.send(Err(error.clone()));
                                let _ = reply.send(Err(error));
                                continue;
                            }

                            compaction_blocked = true;
                            in_turn = true;
                            idle_transition_armed = false;
                            active_compaction = Some(ActiveCompaction {
                                reply,
                                summary: String::new(),
                                max_summary_bytes: max_summary_bytes
                                    .clamp(1, MAX_COMPACTION_SUMMARY_BYTES),
                                error: None,
                            });
                            status_handle
                                .update(|s| {
                                    s.is_thinking = true;
                                    s.turn_completed = false;
                                    s.last_error = None;
                                    s.activity_counter = s.activity_counter.saturating_add(1);
                                })
                                .await;
                            let _ = accepted.send(Ok(()));
                            let outcome = backend
                                .as_ref()
                                .expect("backend must exist while actor is running")
                                .send_with_outcome(AgentInput::SendMessage(
                                    internal_compaction_input(summary_prompt),
                                ))
                                .await;
                            if !matches!(outcome, SendOutcome::Accepted) {
                                let backend_busy = matches!(outcome, SendOutcome::Busy(_));
                                let error = if backend_busy {
                                    "agent backend rejected the compaction summary because it is busy"
                                        .to_owned()
                                } else {
                                    "agent backend closed before compaction could start".to_owned()
                                };
                                let compaction = active_compaction
                                    .take()
                                    .expect("active compaction disappeared after backend send failed");
                                compaction_blocked = false;
                                // Busy means the backend has a live self-started
                                // turn, so its typing events remain responsible
                                // for the next idle transition.
                                if !backend_busy {
                                    in_turn = false;
                                }
                                idle_transition_armed = false;
                                let last_error = error.clone();
                                status_handle
                                    .update(move |s| {
                                        s.is_thinking = backend_busy;
                                        s.turn_completed = !backend_busy;
                                        s.last_error = Some(last_error);
                                        s.activity_counter = s.activity_counter.saturating_add(1);
                                    })
                                    .await;
                                let _ = compaction.reply.send(Err(error));
                            }
                        }
                        AgentCommand::Compact {
                            summary_prompt,
                            max_summary_bytes,
                            reply,
                        } => {
                            let reject = if matches!(lifecycle, ActorLifecycle::Closing) {
                                Some("agent is closing".to_owned())
                            } else if current_start.origin == AgentOrigin::BackendNative {
                                Some("backend-native agents cannot be compacted".to_owned())
                            } else if active_compaction.is_some() || compaction_blocked {
                                Some("agent compaction is already in progress".to_owned())
                            } else if current_session_id.is_none() {
                                Some("agent has no session to compact".to_owned())
                            } else if in_turn {
                                Some("agent is busy".to_owned())
                            } else if !queue.is_empty() {
                                Some("agent has queued work".to_owned())
                            } else {
                                None
                            };
                            if let Some(error) = reject {
                                let _ = reply.send(Err(error));
                                continue;
                            }

                            compaction_blocked = true;
                            in_turn = true;
                            idle_transition_armed = false;
                            active_compaction = Some(ActiveCompaction {
                                reply,
                                summary: String::new(),
                                max_summary_bytes: max_summary_bytes
                                    .clamp(1, MAX_COMPACTION_SUMMARY_BYTES),
                                error: None,
                            });
                            status_handle
                                .update(|s| {
                                    s.is_thinking = true;
                                    s.turn_completed = false;
                                    s.last_error = None;
                                    s.activity_counter = s.activity_counter.saturating_add(1);
                                })
                                .await;
                            let outcome = backend
                                .as_ref()
                                .expect("backend must exist while actor is running")
                                .send_with_outcome(AgentInput::SendMessage(
                                    internal_compaction_input(summary_prompt),
                                ))
                                .await;
                            if !matches!(outcome, SendOutcome::Accepted) {
                                // A compaction prompt is not a user message: on a
                                // busy hand-back it is abandoned with an error
                                // reply (mirroring the pre-send busy rejection),
                                // never queued as conversation input.
                                let backend_busy = matches!(outcome, SendOutcome::Busy(_));
                                let error = if backend_busy {
                                    "agent is busy".to_owned()
                                } else {
                                    "agent backend closed".to_owned()
                                };
                                let compaction = active_compaction
                                    .take()
                                    .expect("active compaction disappeared after backend send failed");
                                compaction_blocked = false;
                                // Busy means the backend has a live self-started
                                // turn: stay in_turn (its typing events arm the
                                // idle transition) instead of declaring idle
                                // against a backend known to be working.
                                if !backend_busy {
                                    in_turn = false;
                                }
                                idle_transition_armed = false;
                                let last_error = error.clone();
                                status_handle
                                    .update(move |s| {
                                        s.is_thinking = backend_busy;
                                        s.turn_completed = !backend_busy;
                                        s.last_error = Some(last_error);
                                        s.activity_counter = s.activity_counter.saturating_add(1);
                                    })
                                    .await;
                                let _ = compaction.reply.send(Err(error));
                            }
                        }
                        AgentCommand::ReleaseCompaction { reply } => {
                            if active_compaction.is_none() {
                                compaction_blocked = false;
                                if matches!(lifecycle, ActorLifecycle::Running) {
                                    accepting_input_task.store(true, Ordering::SeqCst);
                                }
                            }
                            let _ = reply.send(());
                        }
                        AgentCommand::SetName {
                            name,
                            persistence,
                            reply,
                        } => {
                            let applied = apply_agent_name_change(
                                AgentNameChangeContext {
                                    session_store: &session_store,
                                    session_id: current_session_id.as_ref(),
                                    pending_alias: &mut pending_alias,
                                    current_start: &mut current_start,
                                    start_tx: &start_tx,
                                    event_log: &mut event_log,
                                    subscribers: &mut subscribers,
                                },
                                name,
                                persistence,
                            )
                            .await;
                            let _ = reply.send(applied);
                        }
                        AgentCommand::ApplyGeneratedName { result, reply } => {
                            let applied = apply_generated_agent_name(
                                AgentNameChangeContext {
                                    session_store: &session_store,
                                    session_id: current_session_id.as_ref(),
                                    pending_alias: &mut pending_alias,
                                    current_start: &mut current_start,
                                    start_tx: &start_tx,
                                    event_log: &mut event_log,
                                    subscribers: &mut subscribers,
                                },
                                result,
                            )
                            .await;
                            let _ = reply.send(applied);
                        }
                        AgentCommand::ReadOutput {
                            after_seq,
                            limit,
                            reply,
                        } => {
                            let _ = reply.send(output_events_since(&event_log, after_seq, limit));
                        }
                        AgentCommand::ReadLatestOutput { reply } => {
                            let _ = reply.send(Ok(latest_output.output().clone()));
                        }
                        AgentCommand::FetchSessionHistory {
                            before_seq,
                            limit,
                            reply,
                        } => {
                            let window = if before_seq.is_some() {
                                session_history_window(
                                    &event_log,
                                    before_seq,
                                    limit,
                                    Some(&replay_state),
                                )
                            } else {
                                authoritative_session_history_window(
                                    &transcript_store,
                                    current_session_id
                                        .as_ref()
                                        .expect("live agent must have session_id"),
                                    before_seq,
                                    limit,
                                    None,
                                )
                                .await
                                .unwrap_or_else(|| {
                                    session_history_window(
                                        &event_log,
                                        before_seq,
                                        limit,
                                        Some(&replay_state),
                                    )
                                })
                            };
                            let _ = reply.send(window);
                        }
                        AgentCommand::ReadActivityHistory {
                            after_seq,
                            max_events,
                            max_bytes,
                            reply,
                        } => {
                            let _ = reply.send(activity_history_snapshot(
                                &event_log,
                                Some(&replay_state),
                                after_seq,
                                max_events,
                                max_bytes,
                            ));
                        }
                        AgentCommand::ReadUsageSnapshot { reply } => {
                            let _ = reply.send(agent_usage_snapshot_from_tracker(
                                &current_start,
                                &activity_stats,
                            ));
                        }
                        AgentCommand::Interrupt { reply } => {
                            tracing::debug!(
                                agent_id = %current_start.agent_id,
                                ?backend_kind,
                                "interrupting active agent"
                            );
                            // A closing agent is still running, and interrupting
                            // it is exactly what ends the turn the close is
                            // waiting on. Reporting `NotRunning` here used to
                            // both misdescribe the state and withhold the one
                            // action that could unwedge it, so `Closing` falls
                            // through to the real interrupt below.
                            if active_compaction.is_some() || compaction_blocked {
                                let payload =
                                    compaction_input_rejected_payload(&current_start.agent_id);
                                append_event(
                                    &canonical_stream,
                                    &mut event_log,
                                    &mut subscribers,
                                    FrameKind::AgentError,
                                    &payload,
                                )
                                .await;
                                let _ = reply.send(InterruptOutcome::Rejected);
                                continue;
                            }
                            if let Some(flight) = context_compaction.as_ref() {
                                if in_turn
                                    || matches!(
                                        flight.state,
                                        StoredCompactionState::NativeAccepted
                                    )
                                {
                                    let _ = backend
                                        .as_ref()
                                        .expect("backend must exist during compaction interrupt")
                                        .interrupt()
                                        .await;
                                }
                                let _ = actor_tx.send(
                                    AgentCommand::ContextCompactionTerminal {
                                        operation_id: flight.operation_id.clone(),
                                        result: Err(
                                            "context compaction interrupted".to_owned(),
                                        ),
                                    },
                                );
                                let _ = reply.send(InterruptOutcome::Interrupted);
                                continue;
                            }
                            let interrupted = backend
                                .as_ref()
                                .expect("backend must exist while actor is running")
                                .interrupt()
                                .await;
                            if !interrupted {
                                let payload = AgentErrorPayload {
                                    agent_id: current_start.agent_id.clone(),
                                    code: AgentErrorCode::Internal,
                                    message: "agent backend does not support interrupt".to_owned(),
                                    fatal: false,
                                };
                                append_event(
                                    &canonical_stream,
                                    &mut event_log,
                                    &mut subscribers,
                                    FrameKind::AgentError,
                                    &payload,
                                )
                                .await;
                            }
                            let outcome = if interrupted {
                                InterruptOutcome::Interrupted
                            } else {
                                InterruptOutcome::Rejected
                            };
                            let _ = reply.send(outcome);
                        }
                        #[cfg(feature = "test-support")]
                        AgentCommand::ReadMockControl { reply } => {
                            let _ =
                                reply.send(backend.as_ref().and_then(|live| live.mock_control()));
                        }
                        #[cfg(feature = "test-support")]
                        AgentCommand::ForceBackendShutdownForConformance { reply } => {
                            let Some(live_backend) = backend.take() else {
                                let _ = reply.send(false);
                                continue;
                            };
                            live_backend.shutdown()
                                .await;
                            terminalize_live_activity(
                                LiveActivityTerminalContext {
                                    canonical_stream: &canonical_stream,
                                    event_log: &mut event_log,
                                    replay_state: &mut replay_state,
                                    subscribers: &mut subscribers,
                                    open_tool_call_ids: &mut open_tool_call_ids,
                                    pending_tool_response_ids: &mut pending_tool_response_ids,
                                    active_agent_await_ids: &mut active_agent_await_ids,
                                },
                                LiveActivityTerminalStatus::Failed,
                                "backend transport closed",
                            )
                            .await;
                            let _ = reply.send(true);
                            let payload = AgentErrorPayload {
                                agent_id: current_start.agent_id.clone(),
                                code: AgentErrorCode::BackendFailed,
                                message: "agent backend transport closed".to_owned(),
                                fatal: true,
                            };
                            enter_terminal_failure(
                                TerminalFailureContext {
                                    accepting_input: &accepting_input_task,
                                    status_handle: &status_handle,
                                    canonical_stream: &canonical_stream,
                                    event_log: &mut event_log,
                                    replay_state: &mut replay_state,
                                    subscribers: &mut subscribers,
                                    queue: &mut queue,
                                    session_store: &session_store,
                                    compaction: Some(TerminalCompactionFailureContext {
                                        flight: &mut context_compaction,
                                        session_store: &session_store,
                                        session_id: current_session_id
                                            .as_ref()
                                            .expect("live agent must have session_id"),
                                        start: &current_start,
                                        activity_stats: &mut activity_stats,
                                    }),
                                },
                                &payload,
                            )
                            .await;
                            park_terminal_agent(
                                &session_store,
                                &transcript_store,
                                current_session_id.as_ref(),
                                &mut pending_alias,
                                &mut current_start,
                                &start_tx,
                                &mut event_log,
                                &mut latest_output,
                                &mut subscribers,
                                &mut pending_inputs,
                                &mut rx,
                            )
                            .await;
                            return;
                        }
                        AgentCommand::Close { reply } => {
                            accepting_input_task.store(false, Ordering::SeqCst);
                            if matches!(lifecycle, ActorLifecycle::Closing) {
                                let _ = reply.send(());
                                continue;
                            }
                            lifecycle = ActorLifecycle::Closing;
                            close_reply = Some(reply);
                            if let Some(flight) = context_compaction.take() {
                                let session_id = current_session_id
                                    .as_ref()
                                    .expect("live agent must have session_id");
                                let accepted = matches!(
                                    flight.state,
                                    StoredCompactionState::NativeAccepted
                                ) || flight.terminal_taken;
                                let mutation = if accepted {
                                    CompactionMutation::MayHaveMutated
                                } else {
                                    CompactionMutation::NotObserved
                                };
                                let message =
                                    "agent closed during context compaction".to_owned();
                                record_context_compaction_terminal(
                                    flight,
                                    ContextCompactionTerminalRecord {
                                        accepted,
                                        mutation,
                                        method: None,
                                        metrics: CompactionMetrics::default(),
                                        provider_session_id: None,
                                        status:
                                            ContextCompactionTimelineStatus::Failed,
                                        message: Some(message),
                                        trusted_post_context_tokens:
                                            accepted.then_some(None),
                                    },
                                    &session_store,
                                    session_id,
                                    &current_start,
                                    &canonical_stream,
                                    &mut event_log,
                                    &mut replay_state,
                                    &mut subscribers,
                                    &mut activity_stats,
                                    Some(&mut activity_event_seq),
                                )
                                .await;
                            }
                            if !queue.is_empty() {
                                queue.clear();
                                update_queued_messages_snapshot(
                                    &canonical_stream,
                                    &mut event_log,
                                    &mut subscribers,
                                    &queue,
                                    &session_store,
                                )
                                .await;
                            }
                            let waiting_for_user_response = !pending_tool_response_ids.is_empty();
                            if waiting_for_user_response {
                                pending_tool_response_ids.clear();
                            }
                            if !in_turn || waiting_for_user_response {
                                let reply = close_reply
                                    .take()
                                    .expect("close requested without pending close reply");
                                let backend = backend
                                    .take()
                                    .expect("backend must exist while closing a live actor");
                                backend.shutdown().await;
                                abort_resume_replay_barrier_task(&mut resume_replay_barrier_task);
                                terminalize_live_activity(
                                    LiveActivityTerminalContext {
                                        canonical_stream: &canonical_stream,
                                        event_log: &mut event_log,
                                        replay_state: &mut replay_state,
                                        subscribers: &mut subscribers,
                                        open_tool_call_ids: &mut open_tool_call_ids,
                                        pending_tool_response_ids: &mut pending_tool_response_ids,
                                        active_agent_await_ids: &mut active_agent_await_ids,
                                    },
                                    LiveActivityTerminalStatus::Stopped,
                                    "agent closed",
                                )
                                .await;
                                finish_actor_close(&accepting_input_task, &status_handle, reply).await;
                                return;
                            }
                            // Closing a busy agent has to end its turn, not
                            // wait politely for one. A turn can stay open
                            // indefinitely — an orchestrator blocked on
                            // `tyde_await_agents` is the common case — so the
                            // idle transition this close needs may never come
                            // on its own. Interrupt first, then bound the wait.
                            let interrupted = tokio::time::timeout(
                                CLOSE_TURN_GRACE,
                                backend
                                    .as_ref()
                                    .expect("backend must exist while closing a live actor")
                                    .interrupt(),
                            )
                            .await
                            .unwrap_or(false);
                            if !interrupted
                            {
                                tracing::warn!(
                                    agent_id = %current_start.agent_id,
                                    "agent backend does not support interrupt; close will wait for the grace period"
                                );
                            }
                            close_deadline =
                                Some(tokio::time::Instant::now() + CLOSE_TURN_GRACE);
                        }
                        AgentCommand::Attach { stream, reply } => {
                            tracing::debug!(
                                agent_id = %current_start.agent_id,
                                stream = %stream.path(),
                                "attaching stream to active agent"
                            );
                            if resume_replay_gate_pending {
                                pending_resume_attaches.push((stream, reply));
                                continue;
                            }
                            let attached = attach_subscriber_with_latest_output(
                                &event_log,
                                Some(&replay_state),
                                latest_output.output(),
                                status_handle.snapshot().await.is_active(),
                                &mut subscribers,
                                stream,
                            );
                            let _ = reply.send(attached);
                        }
                    }
                }
                () = close_grace_elapsed(&close_deadline) => {
                    // The interrupt issued when this close began never produced
                    // an idle transition. Waiting longer only preserves an
                    // agent the user can no longer cancel, close, or message,
                    // so tear it down and say plainly that we did.
                    let reply = close_reply
                        .take()
                        .expect("close deadline armed without pending close reply");
                    tracing::warn!(
                        agent_id = %current_start.agent_id,
                        grace_ms = CLOSE_TURN_GRACE.as_millis(),
                        "agent turn did not settle after close interrupt; forcing shutdown"
                    );
                    if let Some(backend) = backend.take() {
                        backend.shutdown().await;
                    }
                    abort_resume_replay_barrier_task(&mut resume_replay_barrier_task);
                    terminalize_live_activity(
                        LiveActivityTerminalContext {
                            canonical_stream: &canonical_stream,
                            event_log: &mut event_log,
                            replay_state: &mut replay_state,
                            subscribers: &mut subscribers,
                            open_tool_call_ids: &mut open_tool_call_ids,
                            pending_tool_response_ids: &mut pending_tool_response_ids,
                            active_agent_await_ids: &mut active_agent_await_ids,
                        },
                        LiveActivityTerminalStatus::Stopped,
                        "agent closed",
                    )
                    .await;
                    finish_actor_close(&accepting_input_task, &status_handle, reply).await;
                    return;
                }
            }
        }
    });

    (
        AgentHandle {
            tx,
            accepting_input,
            closing,
            start: start_rx,
        },
        startup_rx,
    )
}

enum AgentStartupEvent<T> {
    Completed(T),
    Command(Box<Option<AgentCommand>>),
}

fn backend_startup_drop_cancels_workers(backend_kind: BackendKind) -> bool {
    // Enabling the command race is safe only when every startup path for the
    // backend explicitly cancels or reaps work after its returned future drops.
    matches!(
        backend_kind,
        BackendKind::Claude
            | BackendKind::Codex
            | BackendKind::Acp
            | BackendKind::Hermes
            | BackendKind::Tycode
    )
}

async fn wait_for_compact_if_inactive_test_gate(_agent_id: &AgentId) {}

async fn wait_for_context_fallback_test_gate(_session_id: &SessionId) {}

async fn next_agent_startup_event<F>(
    startup: Pin<&mut F>,
    rx: &mut mpsc::UnboundedReceiver<AgentCommand>,
    cancellation_supported: bool,
) -> AgentStartupEvent<F::Output>
where
    F: std::future::Future,
{
    tokio::select! {
        biased;
        command = rx.recv(), if cancellation_supported => {
            AgentStartupEvent::Command(Box::new(command))
        },
        result = startup => AgentStartupEvent::Completed(result),
    }
}

pub(crate) struct RelayEventReceivers {
    pub events: mpsc::UnboundedReceiver<ChatEvent>,
    pub model_usage: mpsc::UnboundedReceiver<ModelRequestTokenUsage>,
    pub total_usage: mpsc::UnboundedReceiver<u64>,
}

pub(crate) struct RelayAgentRuntimeResources {
    pub session_store: Arc<Mutex<SessionStore>>,
    pub transcript_store: TranscriptStore,
    pub session_summary_count_tx: HostSessionSummaryCountTx,
}

pub(crate) fn spawn_relay_agent_actor(
    agent_id: AgentId,
    start: AgentStartPayload,
    receivers: RelayEventReceivers,
    runtime: RelayAgentRuntimeResources,
    session_id: SessionId,
    status_handle: registry::AgentStatusHandle,
) -> AgentHandle {
    let RelayEventReceivers {
        mut events,
        mut model_usage,
        mut total_usage,
    } = receivers;
    let RelayAgentRuntimeResources {
        session_store,
        transcript_store,
        session_summary_count_tx,
    } = runtime;
    let (tx, mut rx) = mpsc::unbounded_channel::<AgentCommand>();
    let accepting_input = Arc::new(AtomicBool::new(true));
    let accepting_input_task = Arc::clone(&accepting_input);
    let closing = Arc::new(AtomicBool::new(false));
    let (start_tx, start_rx) = watch::channel(start.clone());

    tokio::spawn(async move {
        let canonical_stream = format!("/agent/{}", agent_id);
        register_transcript_session(&canonical_stream, &session_id, &transcript_store);
        let mut event_log: Vec<Envelope> = Vec::new();
        let mut latest_output = AgentControlLatestOutput::default();
        let mut replay_state = AgentReplayState::default();
        let mut subscribers: Vec<Stream> = Vec::new();
        let mut active_stream_text = String::new();
        let mut activity_stats = AgentActivityStatsTracker::for_backend(start.backend_kind);
        let mut activity_event_seq = 0_u64;
        let mut current_start = start;
        let mut pending_alias = None;
        let mut in_turn = false;
        let mut open_tool_call_ids: HashSet<String> = HashSet::new();
        let mut pending_tool_response_ids: HashSet<String> = HashSet::new();
        let mut active_agent_await_ids: HashSet<String> = HashSet::new();
        let mut lifecycle = ActorLifecycle::Running;
        let mut close_reply: Option<oneshot::Sender<()>> = None;
        let mut close_deadline: Option<tokio::time::Instant> = None;
        let mut model_usage_open = true;
        let mut total_usage_open = true;

        status_handle
            .update(|s| {
                s.started = true;
                s.last_error = None;
                s.activity_counter = s.activity_counter.saturating_add(1);
            })
            .await;
        append_event(
            &canonical_stream,
            &mut event_log,
            &mut subscribers,
            FrameKind::AgentStart,
            &current_start,
        )
        .await;
        upsert_activity_stats_snapshot(
            &canonical_stream,
            &mut event_log,
            &mut subscribers,
            &current_start.agent_id,
            activity_stats.snapshot(),
        )
        .await;

        loop {
            latest_output
                .observe_event_log(&event_log)
                .expect("typed relay replay log must project latest output");
            tokio::select! {
                maybe_usage = model_usage.recv(), if model_usage_open => {
                    let Some(usage) = maybe_usage else {
                        model_usage_open = false;
                        continue;
                    };
                    let source_seq = activity_event_seq;
                    activity_event_seq = activity_event_seq.saturating_add(1);
                    if activity_stats.observe_model_request_token_usage(usage, source_seq) {
                        upsert_activity_stats_snapshot(
                            &canonical_stream,
                            &mut event_log,
                            &mut subscribers,
                            &current_start.agent_id,
                            activity_stats.snapshot(),
                        )
                        .await;
                    }
                }
                maybe_total = total_usage.recv(), if total_usage_open => {
                    let Some(total_tokens) = maybe_total else {
                        total_usage_open = false;
                        continue;
                    };
                    let source_seq = activity_event_seq;
                    activity_event_seq = activity_event_seq.saturating_add(1);
                    if activity_stats.observe_total_only_token_usage(total_tokens, source_seq) {
                        upsert_activity_stats_snapshot(
                            &canonical_stream,
                            &mut event_log,
                            &mut subscribers,
                            &current_start.agent_id,
                            activity_stats.snapshot(),
                        )
                        .await;
                    }
                }
                maybe_event = events.recv() => {
                    let Some(mut event) = maybe_event else {
                        while let Ok(usage) = model_usage.try_recv() {
                            let source_seq = activity_event_seq;
                            activity_event_seq = activity_event_seq.saturating_add(1);
                            if activity_stats.observe_model_request_token_usage(usage, source_seq) {
                                upsert_activity_stats_snapshot(
                                    &canonical_stream,
                                    &mut event_log,
                                    &mut subscribers,
                                    &current_start.agent_id,
                                    activity_stats.snapshot(),
                                )
                                .await;
                            }
                        }
                        while let Ok(total_tokens) = total_usage.try_recv() {
                            let source_seq = activity_event_seq;
                            activity_event_seq = activity_event_seq.saturating_add(1);
                            if activity_stats
                                .observe_total_only_token_usage(total_tokens, source_seq)
                            {
                                upsert_activity_stats_snapshot(
                                    &canonical_stream,
                                    &mut event_log,
                                    &mut subscribers,
                                    &current_start.agent_id,
                                    activity_stats.snapshot(),
                                )
                                .await;
                            }
                        }
                        if matches!(lifecycle, ActorLifecycle::Closing) {
                            let reply = close_reply
                                .take()
                                .expect("close requested without pending close reply");
                            terminalize_live_activity(
                                LiveActivityTerminalContext {
                                    canonical_stream: &canonical_stream,
                                    event_log: &mut event_log,
                                    replay_state: &mut replay_state,
                                    subscribers: &mut subscribers,
                                    open_tool_call_ids: &mut open_tool_call_ids,
                                    pending_tool_response_ids: &mut pending_tool_response_ids,
                                    active_agent_await_ids: &mut active_agent_await_ids,
                                },
                                LiveActivityTerminalStatus::Stopped,
                                "agent closed",
                            )
                            .await;
                            finish_actor_close(&accepting_input_task, &status_handle, reply).await;
                            return;
                        }
                        if status_handle.snapshot().await.is_active() {
                            let idle = ChatEvent::TypingStatusChanged(false);
                            append_chat_event(
                                &canonical_stream,
                                &mut event_log,
                                &mut subscribers,
                                &mut replay_state,
                                &idle,
                            )
                            .await;
                        }
                        accepting_input_task.store(false, Ordering::SeqCst);
                        replay_state.clear_active_stream();
                        status_handle.update(|s| {
                            s.terminated = true;
                            s.is_thinking = false;
                            s.turn_completed = true;
                            s.pending_user_response = None;
                            s.activity_counter = s.activity_counter.saturating_add(1);
                        }).await;
                        // The subagent's backend event stream is done, but the
                        // agent handle is still in the registry. Keep serving
                        // Snapshot/ReadOutput/Attach/SetName so host-stream
                        // registration replay (host::register_host_stream) can
                        // find us, until the host explicitly closes the agent.
                        park_relay_terminal_agent(
                            &session_store,
                            &transcript_store,
                            &session_id,
                            &mut pending_alias,
                            &mut current_start,
                            &start_tx,
                            &mut event_log,
                            &mut latest_output,
                            &mut subscribers,
                            &mut rx,
                            &accepting_input_task,
                            &status_handle,
                            &canonical_stream,
                        )
                        .await;
                        return;
                    };

                    match &event {
                        ChatEvent::MessageAdded(message) => {
                            if matches!(message.sender, MessageSender::Error) {
                                let msg = message.content.clone();
                                status_handle.update(|s| {
                                    s.turn_completed = true;
                                    s.last_error = Some(msg);
                                    s.activity_counter = s.activity_counter.saturating_add(1);
                                }).await;
                            } else {
                                status_handle.update(|s| {
                                    s.activity_counter = s.activity_counter.saturating_add(1);
                                }).await;
                            }
                        }
                        ChatEvent::StreamStart(_) => {
                            active_stream_text.clear();
                            in_turn = true;
                            status_handle.update(|s| {
                                s.last_error = None;
                                s.activity_counter = s.activity_counter.saturating_add(1);
                            }).await;
                        }
                        ChatEvent::StreamDelta(delta) => active_stream_text.push_str(&delta.text),
                        ChatEvent::StreamEnd(_) => {
                            active_stream_text.clear();
                            status_handle.update(|s| {
                                s.turn_completed = true;
                                s.last_error = None;
                                s.activity_counter = s.activity_counter.saturating_add(1);
                            }).await;
                        }
                        ChatEvent::TypingStatusChanged(typing) => {
                            let typing = *typing;
                            in_turn = typing;
                            status_handle.update(|s| {
                                s.is_thinking = typing;
                                s.turn_completed = !typing;
                                s.activity_counter = s.activity_counter.saturating_add(1);
                            }).await;
                        }
                        ChatEvent::OperationCancelled(_) => {
                            pending_tool_response_ids.clear();
                            status_handle.update(|s| {
                                s.pending_user_response = None;
                                s.is_thinking = false;
                                s.turn_completed = true;
                                s.activity_counter = s.activity_counter.saturating_add(1);
                            }).await;
                        }
                        ChatEvent::ToolRequest(request) => {
                            open_tool_call_ids.insert(request.tool_call_id.clone());
                            if matches!(
                                &request.tool_type,
                                protocol::ToolRequestType::TydeAwaitAgents { .. }
                            ) {
                                active_agent_await_ids.insert(request.tool_call_id.clone());
                            }
                            let waiting_for_plan_approval = matches!(
                                &request.tool_type,
                                protocol::ToolRequestType::ExitPlanMode { .. }
                            );
                            if waiting_for_plan_approval {
                                pending_tool_response_ids.insert(request.tool_call_id.clone());
                            }
                            status_handle.update(|s| {
                                if waiting_for_plan_approval {
                                    s.pending_user_response =
                                        Some(registry::PendingUserResponseKind::PlanApproval);
                                }
                                s.activity_counter = s.activity_counter.saturating_add(1);
                            }).await;
                        }
                        ChatEvent::ToolExecutionCompleted(completion) => {
                            open_tool_call_ids.remove(&completion.tool_call_id);
                            active_agent_await_ids.remove(&completion.tool_call_id);
                            let completed_pending_response =
                                pending_tool_response_ids.remove(&completion.tool_call_id);
                            status_handle.update(|s| {
                                if completed_pending_response && pending_tool_response_ids.is_empty() {
                                    s.pending_user_response = None;
                                    s.turn_completed = false;
                                    s.is_thinking = true;
                                }
                                s.activity_counter = s.activity_counter.saturating_add(1);
                            }).await;
                        }
                        _ => {
                            status_handle.update(|s| {
                                s.activity_counter = s.activity_counter.saturating_add(1);
                            }).await;
                        }
                    }

                    if let Some(update) =
                        apply_runtime_session_updates(&session_store, &session_id, &event).await
                    {
                        let _ = session_summary_count_tx.send(
                            HostSessionSummaryCountEvent::Update(
                                HostSessionSummaryCountUpdate {
                                    agent_id: agent_id.clone(),
                                    payload: update,
                                },
                            ),
                        );
                    }
                    let source_seq = activity_event_seq;
                    activity_event_seq = activity_event_seq.saturating_add(1);
                    if activity_stats.observe_chat_event(
                        &mut event,
                        source_seq,
                        &active_stream_text,
                    ) {
                        upsert_activity_stats_snapshot(
                            &canonical_stream,
                            &mut event_log,
                            &mut subscribers,
                            &current_start.agent_id,
                            activity_stats.snapshot(),
                        )
                        .await;
                    }
                    append_chat_event(
                        &canonical_stream,
                        &mut event_log,
                        &mut subscribers,
                        &mut replay_state,
                        &event,
                    )
                    .await;

                    if matches!(event, ChatEvent::TypingStatusChanged(false))
                        && matches!(lifecycle, ActorLifecycle::Closing)
                    {
                        let reply = close_reply
                            .take()
                            .expect("close requested without pending close reply");
                        terminalize_live_activity(
                            LiveActivityTerminalContext {
                                canonical_stream: &canonical_stream,
                                event_log: &mut event_log,
                                replay_state: &mut replay_state,
                                subscribers: &mut subscribers,
                                open_tool_call_ids: &mut open_tool_call_ids,
                                pending_tool_response_ids: &mut pending_tool_response_ids,
                                active_agent_await_ids: &mut active_agent_await_ids,
                            },
                            LiveActivityTerminalStatus::Stopped,
                            "agent closed",
                        )
                        .await;
                        finish_actor_close(&accepting_input_task, &status_handle, reply).await;
                        return;
                    }
                }
                maybe_command = rx.recv() => {
                    let Some(command) = maybe_command else {
                        return;
                    };
                    match command {
                        AgentCommand::ResumeReplayBarrier { .. } => {}
                        AgentCommand::Compact { reply, .. } => {
                            let _ = reply.send(Err("backend-native agents cannot be compacted".to_owned()));
                        }
                        AgentCommand::CompactIfInactive { accepted, reply, .. } => {
                            let error = "backend-native agents cannot be compacted".to_owned();
                            let _ = accepted.send(Err(error.clone()));
                            let _ = reply.send(Err(error));
                        }
                        AgentCommand::ReadCompactionCapability { reply } => {
                            let _ = reply.send(
                                crate::backend::BackendCompactionCapability::default(),
                            );
                        }
                        AgentCommand::ReadRequestedCompactionRoute { reply, .. } => {
                            let _ = reply.send(Err(
                                "backend-native relay agents cannot be compacted".to_owned(),
                            ));
                        }
                        AgentCommand::RequestContextCompaction { reply, .. } => {
                            let _ = reply.send(Err(
                                "backend-native relay agents cannot be compacted".to_owned(),
                            ));
                        }
                        AgentCommand::ContextCompactionFallbackPrepared { result, .. } => {
                            if let Ok(prepared) = result {
                                prepared.binding.backend.shutdown().await;
                            }
                        }
                        AgentCommand::ContextCompactionTerminal { .. }
                        | AgentCommand::RetryContextCompaction { .. }
                        | AgentCommand::ContextCompactionBarrierExpired { .. } => {}
                        AgentCommand::ReleaseCompaction { reply } => {
                            let _ = reply.send(());
                        }
                        AgentCommand::SendInput(_) => {
                            let payload = relay_input_rejected_payload(&current_start.agent_id);
                            append_event(
                                &canonical_stream,
                                &mut event_log,
                                &mut subscribers,
                                FrameKind::AgentError,
                                &payload,
                            )
                            .await;
                        }
                        AgentCommand::DeliverMessage { reply, .. } => {
                            // A relay mirrors a backend-native child and never
                            // accepts direct input. Answering the caller is the
                            // whole fix: the old path left the target marked
                            // active for a message it would never run.
                            let _ = reply.send(Err(DELIVERY_REJECTED_RELAY.to_owned()));
                        }
                        AgentCommand::Interrupt { reply } => {
                            let payload = relay_input_rejected_payload(&current_start.agent_id);
                            append_event(
                                &canonical_stream,
                                &mut event_log,
                                &mut subscribers,
                                FrameKind::AgentError,
                                &payload,
                            )
                            .await;
                            let _ = reply.send(InterruptOutcome::Rejected);
                        }
                        AgentCommand::SetName {
                            name,
                            persistence,
                            reply,
                        } => {
                            let applied = apply_agent_name_change(
                                AgentNameChangeContext {
                                    session_store: &session_store,
                                    session_id: Some(&session_id),
                                    pending_alias: &mut pending_alias,
                                    current_start: &mut current_start,
                                    start_tx: &start_tx,
                                    event_log: &mut event_log,
                                    subscribers: &mut subscribers,
                                },
                                name,
                                persistence,
                            )
                            .await;
                            let _ = reply.send(applied);
                        }
                        AgentCommand::ApplyGeneratedName { result, reply } => {
                            let applied = apply_generated_agent_name(
                                AgentNameChangeContext {
                                    session_store: &session_store,
                                    session_id: Some(&session_id),
                                    pending_alias: &mut pending_alias,
                                    current_start: &mut current_start,
                                    start_tx: &start_tx,
                                    event_log: &mut event_log,
                                    subscribers: &mut subscribers,
                                },
                                result,
                            )
                            .await;
                            let _ = reply.send(applied);
                        }
                        AgentCommand::ReadOutput {
                            after_seq,
                            limit,
                            reply,
                        } => {
                            let _ = reply.send(output_events_since(&event_log, after_seq, limit));
                        }
                        AgentCommand::ReadLatestOutput { reply } => {
                            let _ = reply.send(Ok(latest_output.output().clone()));
                        }
                        AgentCommand::FetchSessionHistory {
                            before_seq,
                            limit,
                            reply,
                        } => {
                            let window = if before_seq.is_some() {
                                session_history_window(
                                    &event_log,
                                    before_seq,
                                    limit,
                                    Some(&replay_state),
                                )
                            } else {
                                authoritative_session_history_window(
                                    &transcript_store,
                                    &session_id,
                                    before_seq,
                                    limit,
                                    None,
                                )
                                .await
                                .unwrap_or_else(|| {
                                    session_history_window(
                                        &event_log,
                                        before_seq,
                                        limit,
                                        Some(&replay_state),
                                    )
                                })
                            };
                            let _ = reply.send(window);
                        }
                        AgentCommand::ReadActivityHistory {
                            after_seq,
                            max_events,
                            max_bytes,
                            reply,
                        } => {
                            let _ = reply.send(activity_history_snapshot(
                                &event_log,
                                Some(&replay_state),
                                after_seq,
                                max_events,
                                max_bytes,
                            ));
                        }
                        AgentCommand::ReadUsageSnapshot { reply } => {
                            let _ = reply.send(agent_usage_snapshot_from_tracker(
                                &current_start,
                                &activity_stats,
                            ));
                        }
                        AgentCommand::Close { reply } => {
                            accepting_input_task.store(false, Ordering::SeqCst);
                            if matches!(lifecycle, ActorLifecycle::Closing) {
                                let _ = reply.send(());
                                continue;
                            }
                            lifecycle = ActorLifecycle::Closing;
                            close_reply = Some(reply);
                            let waiting_for_user_response = !pending_tool_response_ids.is_empty();
                            if waiting_for_user_response {
                                pending_tool_response_ids.clear();
                            }
                            if !in_turn || waiting_for_user_response {
                                let reply = close_reply
                                    .take()
                                    .expect("close requested without pending close reply");
                                terminalize_live_activity(
                                    LiveActivityTerminalContext {
                                        canonical_stream: &canonical_stream,
                                        event_log: &mut event_log,
                                        replay_state: &mut replay_state,
                                        subscribers: &mut subscribers,
                                        open_tool_call_ids: &mut open_tool_call_ids,
                                        pending_tool_response_ids: &mut pending_tool_response_ids,
                                        active_agent_await_ids: &mut active_agent_await_ids,
                                    },
                                    LiveActivityTerminalStatus::Stopped,
                                    "agent closed",
                                )
                                .await;
                                finish_actor_close(&accepting_input_task, &status_handle, reply).await;
                                return;
                            }
                            // A relay mirrors a backend-native child and owns no
                            // backend to interrupt, so the deadline is the only
                            // guarantee it has. Without it a mirrored turn that
                            // never reports idle parks the relay in `Closing`
                            // for good.
                            close_deadline =
                                Some(tokio::time::Instant::now() + CLOSE_TURN_GRACE);
                        }
                        // Relay agents mirror a backend-native child and own
                        // no backend, mock or otherwise.
                        #[cfg(feature = "test-support")]
                        AgentCommand::ReadMockControl { reply } => {
                            let _ = reply.send(None);
                        }
                        #[cfg(feature = "test-support")]
                        AgentCommand::ForceBackendShutdownForConformance { reply } => {
                            accepting_input_task.store(false, Ordering::SeqCst);
                            status_handle
                                .update(|status| {
                                    status.terminated = true;
                                    status.is_thinking = false;
                                    status.turn_completed = true;
                                    status.pending_user_response = None;
                                    status.last_error = Some(
                                        "backend-native child transport closed".to_owned(),
                                    );
                                    status.activity_counter =
                                        status.activity_counter.saturating_add(1);
                                })
                                .await;
                            let payload = AgentErrorPayload {
                                agent_id: current_start.agent_id.clone(),
                                code: AgentErrorCode::BackendFailed,
                                message: "backend-native child transport closed".to_owned(),
                                fatal: true,
                            };
                            append_event(
                                &canonical_stream,
                                &mut event_log,
                                &mut subscribers,
                                FrameKind::AgentError,
                                &payload,
                            )
                            .await;
                            let _ = reply.send(true);
                            return;
                        }
                        AgentCommand::Attach { stream, reply } => {
                            let attached = attach_subscriber_with_latest_output(
                                &event_log,
                                Some(&replay_state),
                                latest_output.output(),
                                status_handle.snapshot().await.is_active(),
                                &mut subscribers,
                                stream,
                            );
                            let _ = reply.send(attached);
                        }
                    }
                }
                () = close_grace_elapsed(&close_deadline) => {
                    // The mirrored turn never reported idle. Holding the relay
                    // open only preserves a row the user cannot dismiss.
                    let reply = close_reply
                        .take()
                        .expect("close deadline armed without pending close reply");
                    tracing::warn!(
                        agent_id = %current_start.agent_id,
                        grace_ms = CLOSE_TURN_GRACE.as_millis(),
                        "relay turn did not settle after close; forcing shutdown"
                    );
                    terminalize_live_activity(
                        LiveActivityTerminalContext {
                            canonical_stream: &canonical_stream,
                            event_log: &mut event_log,
                            replay_state: &mut replay_state,
                            subscribers: &mut subscribers,
                            open_tool_call_ids: &mut open_tool_call_ids,
                            pending_tool_response_ids: &mut pending_tool_response_ids,
                            active_agent_await_ids: &mut active_agent_await_ids,
                        },
                        LiveActivityTerminalStatus::Stopped,
                        "agent closed",
                    )
                    .await;
                    finish_actor_close(&accepting_input_task, &status_handle, reply).await;
                    return;
                }
            }
        }
    });

    AgentHandle {
        tx,
        accepting_input,
        closing,
        start: start_rx,
    }
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time is before UNIX_EPOCH")
        .as_millis() as u64
}

async fn run_session_store_io<T, F>(
    session_store: &Arc<Mutex<SessionStore>>,
    operation: F,
) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce(&SessionStore) -> Result<T, String> + Send + 'static,
{
    let session_store = Arc::clone(session_store);
    tokio::task::spawn_blocking(move || {
        let store = session_store.blocking_lock();
        operation(&store)
    })
    .await
    .map_err(|error| format!("session persistence task failed: {error}"))?
}

async fn transcript_is_authoritative(store: &TranscriptStore, session_id: &SessionId) -> bool {
    if !store.actor_io_enabled() {
        return false;
    }
    let session_id = session_id.clone();
    let store = store.clone();
    tokio::task::spawn_blocking(move || store.is_authoritative(&session_id))
        .await
        .unwrap_or(false)
}

/// Renders a stall window the way a person would read it: whole minutes when
/// the configured value is whole minutes, seconds otherwise.
fn stall_timeout_label(seconds: u32) -> String {
    if seconds >= 60 && seconds.is_multiple_of(60) {
        let minutes = seconds / 60;
        if minutes == 1 {
            return "1 minute".to_owned();
        }
        return format!("{minutes} minutes");
    }
    if seconds == 1 {
        return "1 second".to_owned();
    }
    format!("{seconds} seconds")
}

fn supervisor_stall_interrupt_notice_event(stall_timeout_seconds: u32) -> ChatEvent {
    ChatEvent::MessageAdded(ChatMessage {
        message_id: None,
        timestamp: now_ms(),
        sender: MessageSender::Warning,
        content: format!(
            "{} {}. The supervisor is deciding how to make progress.",
            protocol::SUPERVISOR_STALL_INTERRUPT_NOTICE_PREFIX,
            stall_timeout_label(stall_timeout_seconds)
        ),
        reasoning: None,
        tool_calls: Vec::new(),
        model_info: None,
        token_usage: None,
        context_breakdown: None,
        images: None,
    })
}

fn supervisor_failure_warning_event(attempts_started: u8) -> ChatEvent {
    let attempt_label = if attempts_started == 1 {
        "attempt"
    } else {
        "attempts"
    };
    ChatEvent::MessageAdded(ChatMessage {
        message_id: None,
        timestamp: now_ms(),
        sender: MessageSender::Warning,
        content: format!(
            "Supervisor could not verify whether this task was complete after {attempts_started} {attempt_label} and has stopped retrying. Send a follow-up message if you want the agent to continue."
        ),
        reasoning: None,
        tool_calls: Vec::new(),
        model_info: None,
        token_usage: None,
        context_breakdown: None,
        images: None,
    })
}

/// Resolves when a pending close's grace period expires.
///
/// Stays pending forever while `deadline` is `None`, so the select arm this
/// feeds is inert outside [`ActorLifecycle::Closing`] and never competes with
/// the backend-event or command arms during normal operation.
async fn close_grace_elapsed(deadline: &Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(*deadline).await,
        None => std::future::pending().await,
    }
}

async fn finish_actor_close(
    accepting_input: &Arc<AtomicBool>,
    status_handle: &registry::AgentStatusHandle,
    reply: oneshot::Sender<()>,
) {
    accepting_input.store(false, Ordering::SeqCst);
    status_handle
        .update(|s| {
            s.terminated = true;
            s.is_thinking = false;
            s.turn_completed = true;
            s.pending_user_response = None;
            s.activity_counter = s.activity_counter.saturating_add(1);
        })
        .await;
    let _ = reply.send(());
}

/// Rejection reasons for an acknowledged [`AgentCommand::DeliverMessage`].
///
/// These reach the agent-control caller as the tool's error text, so they
/// deliberately mirror the wording of the transcript rejections the
/// fire-and-forget path appends for the same states. A rejected acknowledged
/// delivery appends nothing: the caller is told directly, and inventing a
/// transcript error for a message the actor never accepted would put a failure
/// in the target's history that its user never caused.
const DELIVERY_REJECTED_RELAY: &str = "backend-native relay agents do not accept direct input";
const DELIVERY_REJECTED_COMPACTING: &str = "agent compaction is in progress";
const DELIVERY_REJECTED_CLOSING: &str = "agent is closing";
const DELIVERY_REJECTED_TERMINAL: &str = "agent not running";
const DELIVERY_REJECTED_BACKEND_CLOSED: &str = "agent backend closed";
const DELIVERY_REJECTED_MAILBOX_CLOSED: &str = "agent backend is closed";
const DELIVERY_NOT_ACKNOWLEDGED: &str = "agent actor did not acknowledge the message";

/// Resolves an acknowledged delivery as rejected.
///
/// Returns whether there was an acknowledgement to resolve, so a shared
/// rejection site can skip the transcript error it would otherwise append for
/// fire-and-forget input.
fn reject_agent_delivery(ack: Option<oneshot::Sender<Result<(), String>>>, reason: &str) -> bool {
    let Some(reply) = ack else {
        return false;
    };
    let _ = reply.send(Err(reason.to_owned()));
    true
}

fn relay_input_rejected_payload(agent_id: &AgentId) -> AgentErrorPayload {
    AgentErrorPayload {
        agent_id: agent_id.clone(),
        code: AgentErrorCode::Internal,
        message: "backend-native relay agents do not accept direct input".to_owned(),
        fatal: false,
    }
}

fn terminal_input_rejected_payload(agent_id: &AgentId) -> AgentErrorPayload {
    AgentErrorPayload {
        agent_id: agent_id.clone(),
        code: AgentErrorCode::Internal,
        message: "agent not running".to_owned(),
        fatal: false,
    }
}

fn closing_input_rejected_payload(agent_id: &AgentId) -> AgentErrorPayload {
    AgentErrorPayload {
        agent_id: agent_id.clone(),
        code: AgentErrorCode::Internal,
        message: DELIVERY_REJECTED_CLOSING.to_owned(),
        fatal: false,
    }
}

fn compaction_input_rejected_payload(agent_id: &AgentId) -> AgentErrorPayload {
    AgentErrorPayload {
        agent_id: agent_id.clone(),
        code: AgentErrorCode::Internal,
        message: "agent compaction is in progress".to_owned(),
        fatal: false,
    }
}

fn stale_tool_response_rejected_event() -> ChatEvent {
    tool_response_rejected_event("No matching pending tool request for this response")
}

fn tool_response_rejected_event(message: &str) -> ChatEvent {
    ChatEvent::MessageAdded(ChatMessage {
        message_id: None,
        timestamp: now_ms(),
        sender: MessageSender::Error,
        content: message.to_owned(),
        reasoning: None,
        tool_calls: Vec::new(),
        model_info: None,
        token_usage: None,
        context_breakdown: None,
        images: None,
    })
}

fn push_summary_capped(summary: &mut String, text: &str, max_summary_bytes: usize) {
    let remaining = max_summary_bytes.saturating_sub(summary.len());
    if remaining == 0 {
        return;
    }
    if text.len() <= remaining {
        summary.push_str(text);
        return;
    }
    let mut end = 0;
    for (index, ch) in text.char_indices() {
        let next = index + ch.len_utf8();
        if next > remaining {
            break;
        }
        end = next;
    }
    if end > 0 {
        summary.push_str(&text[..end]);
    }
}

fn complete_compaction(
    compaction: ActiveCompaction,
    session_id: &SessionId,
) -> (
    oneshot::Sender<Result<CompactionSummary, String>>,
    Result<CompactionSummary, String>,
) {
    let reply = compaction.reply;
    if let Some(error) = compaction.error {
        return (reply, Err(error));
    }
    let summary = compaction.summary.trim().to_owned();
    if summary.is_empty() {
        return (reply, Err("compaction summary was empty".to_owned()));
    }
    (
        reply,
        Ok(CompactionSummary {
            session_id: session_id.clone(),
            summary,
        }),
    )
}

async fn enter_terminal_failure(
    mut context: TerminalFailureContext<'_>,
    payload: &AgentErrorPayload,
) {
    if let Some(compaction) = context.compaction.as_mut()
        && let Some(flight) = compaction.flight.take()
    {
        let accepted =
            matches!(flight.state, StoredCompactionState::NativeAccepted) || flight.terminal_taken;
        let mutation = if accepted {
            CompactionMutation::MayHaveMutated
        } else {
            CompactionMutation::NotObserved
        };
        record_context_compaction_terminal(
            flight,
            ContextCompactionTerminalRecord {
                accepted,
                mutation,
                method: None,
                metrics: CompactionMetrics::default(),
                provider_session_id: None,
                status: ContextCompactionTimelineStatus::Failed,
                message: Some(payload.message.clone()),
                trusted_post_context_tokens: accepted.then_some(None),
            },
            compaction.session_store,
            compaction.session_id,
            compaction.start,
            context.canonical_stream,
            context.event_log,
            context.replay_state,
            context.subscribers,
            compaction.activity_stats,
            None,
        )
        .await;
    }
    context.accepting_input.store(false, Ordering::SeqCst);
    context.replay_state.clear_active_stream();
    context.queue.clear();
    context
        .status_handle
        .update(|s| {
            s.terminated = true;
            s.is_thinking = false;
            s.turn_completed = true;
            s.pending_user_response = None;
            s.last_error = Some(payload.message.clone());
            s.activity_counter = s.activity_counter.saturating_add(1);
        })
        .await;
    update_queued_messages_snapshot(
        context.canonical_stream,
        context.event_log,
        context.subscribers,
        context.queue,
        context.session_store,
    )
    .await;
    append_event(
        context.canonical_stream,
        context.event_log,
        context.subscribers,
        FrameKind::AgentError,
        payload,
    )
    .await;
}

#[derive(Clone, Copy)]
enum LiveActivityTerminalStatus {
    Failed,
    Stopped,
}

struct LiveActivityTerminalContext<'a> {
    canonical_stream: &'a str,
    event_log: &'a mut Vec<Envelope>,
    replay_state: &'a mut AgentReplayState,
    subscribers: &'a mut Vec<Stream>,
    open_tool_call_ids: &'a mut HashSet<String>,
    pending_tool_response_ids: &'a mut HashSet<String>,
    active_agent_await_ids: &'a mut HashSet<String>,
}

async fn terminalize_live_activity(
    mut context: LiveActivityTerminalContext<'_>,
    status: LiveActivityTerminalStatus,
    message: &str,
) {
    let canonical_stream = context.canonical_stream;
    let event_log = &mut context.event_log;
    let replay_state = &mut context.replay_state;
    let subscribers = &mut context.subscribers;
    let open_tool_call_ids = &mut context.open_tool_call_ids;
    let pending_tool_response_ids = &mut context.pending_tool_response_ids;
    let active_agent_await_ids = &mut context.active_agent_await_ids;

    let open_tools = open_tool_call_ids.drain().collect::<Vec<_>>();
    for tool_call_id in open_tools {
        pending_tool_response_ids.remove(&tool_call_id);
        let outcome = match status {
            LiveActivityTerminalStatus::Failed => ToolExecutionOutcome::Failed {
                message: message.to_owned(),
                details: Some(message.to_owned()),
                normalization_failure: None,
            },
            LiveActivityTerminalStatus::Stopped => ToolExecutionOutcome::Cancelled {
                message: message.to_owned(),
            },
        };
        append_chat_event(
            canonical_stream,
            event_log,
            subscribers,
            replay_state,
            &ChatEvent::ToolExecutionCompleted(ToolExecutionCompletedData {
                tool_call_id,
                outcome,
            }),
        )
        .await;
    }
    open_tool_call_ids.clear();
    pending_tool_response_ids.clear();
    active_agent_await_ids.clear();
    if replay_state.typing {
        if !replay_state.operation_cancelled {
            append_chat_event(
                canonical_stream,
                event_log,
                subscribers,
                replay_state,
                &ChatEvent::OperationCancelled(protocol::OperationCancelledData {
                    message: message.to_owned(),
                }),
            )
            .await;
        }
        append_chat_event(
            canonical_stream,
            event_log,
            subscribers,
            replay_state,
            &ChatEvent::TypingStatusChanged(false),
        )
        .await;
    }
}

async fn next_agent_command(
    pending_inputs: &mut VecDeque<AgentInput>,
    rx: &mut mpsc::UnboundedReceiver<AgentCommand>,
    drain_pending: bool,
) -> Option<AgentCommand> {
    if drain_pending && let Some(input) = pending_inputs.pop_front() {
        return Some(AgentCommand::SendInput(input));
    }
    rx.recv().await
}

#[allow(clippy::too_many_arguments)]
async fn park_terminal_agent(
    session_store: &Arc<Mutex<SessionStore>>,
    transcript_store: &TranscriptStore,
    session_id: Option<&SessionId>,
    pending_alias: &mut Option<InitialAgentAlias>,
    current_start: &mut AgentStartPayload,
    start_tx: &watch::Sender<AgentStartPayload>,
    event_log: &mut Vec<Envelope>,
    latest_output: &mut AgentControlLatestOutput,
    subscribers: &mut Vec<Stream>,
    pending_inputs: &mut VecDeque<AgentInput>,
    rx: &mut mpsc::UnboundedReceiver<AgentCommand>,
) {
    loop {
        latest_output
            .observe_event_log(event_log)
            .expect("typed terminal replay log must project latest output");
        let Some(command) = next_agent_command(pending_inputs, rx, true).await else {
            break;
        };
        match command {
            AgentCommand::ResumeReplayBarrier { .. } => {}
            AgentCommand::SetName {
                name,
                persistence,
                reply,
            } => {
                let applied = apply_agent_name_change(
                    AgentNameChangeContext {
                        session_store,
                        session_id,
                        pending_alias,
                        current_start,
                        start_tx,
                        event_log,
                        subscribers,
                    },
                    name,
                    persistence,
                )
                .await;
                let _ = reply.send(applied);
            }
            AgentCommand::ApplyGeneratedName { result, reply } => {
                let applied = apply_generated_agent_name(
                    AgentNameChangeContext {
                        session_store,
                        session_id,
                        pending_alias,
                        current_start,
                        start_tx,
                        event_log,
                        subscribers,
                    },
                    result,
                )
                .await;
                let _ = reply.send(applied);
            }
            AgentCommand::ReadOutput {
                after_seq,
                limit,
                reply,
            } => {
                let _ = reply.send(output_events_since(event_log, after_seq, limit));
            }
            AgentCommand::ReadLatestOutput { reply } => {
                let _ = reply.send(Ok(latest_output.output().clone()));
            }
            AgentCommand::FetchSessionHistory {
                before_seq,
                limit,
                reply,
            } => {
                let window = if before_seq.is_some() {
                    session_history_window(event_log, before_seq, limit, None)
                } else {
                    let authoritative = match session_id {
                        Some(session_id) => {
                            authoritative_session_history_window(
                                transcript_store,
                                session_id,
                                before_seq,
                                limit,
                                None,
                            )
                            .await
                        }
                        None => None,
                    };
                    authoritative.unwrap_or_else(|| {
                        session_history_window(event_log, before_seq, limit, None)
                    })
                };
                let _ = reply.send(window);
            }
            AgentCommand::ReadActivityHistory {
                after_seq,
                max_events,
                max_bytes,
                reply,
            } => {
                let _ = reply.send(activity_history_snapshot(
                    event_log, None, after_seq, max_events, max_bytes,
                ));
            }
            AgentCommand::ReadUsageSnapshot { reply } => {
                let _ = reply.send(agent_usage_snapshot_from_log(current_start, event_log));
            }
            AgentCommand::Attach { stream, reply } => {
                let attached = attach_subscriber_with_latest_output(
                    event_log,
                    None,
                    latest_output.output(),
                    false,
                    subscribers,
                    stream,
                );
                let _ = reply.send(attached);
            }
            AgentCommand::Close { reply } => {
                let _ = reply.send(());
                break;
            }
            AgentCommand::Compact { reply, .. } => {
                let _ = reply.send(Err("agent is not running".to_owned()));
            }
            AgentCommand::CompactIfInactive {
                accepted, reply, ..
            } => {
                let error = "agent is not running".to_owned();
                let _ = accepted.send(Err(error.clone()));
                let _ = reply.send(Err(error));
            }
            AgentCommand::ReadCompactionCapability { reply } => {
                let _ = reply.send(crate::backend::BackendCompactionCapability::default());
            }
            AgentCommand::ReadRequestedCompactionRoute { reply, .. } => {
                let _ = reply.send(Err("agent is not running".to_owned()));
            }
            AgentCommand::RequestContextCompaction { reply, .. } => {
                let _ = reply.send(Err("agent is not running".to_owned()));
            }
            AgentCommand::ContextCompactionFallbackPrepared { result, .. } => {
                if let Ok(prepared) = result {
                    prepared.binding.backend.shutdown().await;
                }
            }
            AgentCommand::ContextCompactionTerminal { .. }
            | AgentCommand::RetryContextCompaction { .. }
            | AgentCommand::ContextCompactionBarrierExpired { .. } => {}
            AgentCommand::ReleaseCompaction { reply } => {
                let _ = reply.send(());
            }
            AgentCommand::SendInput(_) => {
                let payload = terminal_input_rejected_payload(&current_start.agent_id);
                append_event(
                    &format!("/agent/{}", current_start.agent_id),
                    event_log,
                    subscribers,
                    FrameKind::AgentError,
                    &payload,
                )
                .await;
            }
            AgentCommand::DeliverMessage { reply, .. } => {
                // The fire-and-forget arm above answers a client with a typed
                // transcript rejection because a human is watching this chat.
                // An acknowledged delivery has a caller to answer instead, and
                // appending here would overwrite the fatal error that explains
                // why the agent is parked.
                let _ = reply.send(Err(DELIVERY_REJECTED_TERMINAL.to_owned()));
            }
            AgentCommand::Interrupt { reply } => {
                let _ = reply.send(InterruptOutcome::NotRunning);
            }
            #[cfg(feature = "test-support")]
            AgentCommand::ForceBackendShutdownForConformance { reply } => {
                let _ = reply.send(false);
            }
            #[cfg(feature = "test-support")]
            AgentCommand::ReadMockControl { reply } => {
                let _ = reply.send(None);
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn park_relay_terminal_agent(
    session_store: &Arc<Mutex<SessionStore>>,
    transcript_store: &TranscriptStore,
    session_id: &SessionId,
    pending_alias: &mut Option<InitialAgentAlias>,
    current_start: &mut AgentStartPayload,
    start_tx: &watch::Sender<AgentStartPayload>,
    event_log: &mut Vec<Envelope>,
    latest_output: &mut AgentControlLatestOutput,
    subscribers: &mut Vec<Stream>,
    rx: &mut mpsc::UnboundedReceiver<AgentCommand>,
    accepting_input: &Arc<AtomicBool>,
    status_handle: &registry::AgentStatusHandle,
    canonical_stream: &str,
) {
    loop {
        latest_output
            .observe_event_log(event_log)
            .expect("typed relay terminal replay log must project latest output");
        let Some(command) = rx.recv().await else {
            break;
        };
        match command {
            AgentCommand::ResumeReplayBarrier { .. } => {}
            AgentCommand::SetName {
                name,
                persistence,
                reply,
            } => {
                let applied = apply_agent_name_change(
                    AgentNameChangeContext {
                        session_store,
                        session_id: Some(session_id),
                        pending_alias,
                        current_start,
                        start_tx,
                        event_log,
                        subscribers,
                    },
                    name,
                    persistence,
                )
                .await;
                let _ = reply.send(applied);
            }
            AgentCommand::ApplyGeneratedName { result, reply } => {
                let applied = apply_generated_agent_name(
                    AgentNameChangeContext {
                        session_store,
                        session_id: Some(session_id),
                        pending_alias,
                        current_start,
                        start_tx,
                        event_log,
                        subscribers,
                    },
                    result,
                )
                .await;
                let _ = reply.send(applied);
            }
            AgentCommand::ReadOutput {
                after_seq,
                limit,
                reply,
            } => {
                let _ = reply.send(output_events_since(event_log, after_seq, limit));
            }
            AgentCommand::ReadLatestOutput { reply } => {
                let _ = reply.send(Ok(latest_output.output().clone()));
            }
            AgentCommand::FetchSessionHistory {
                before_seq,
                limit,
                reply,
            } => {
                let window = if before_seq.is_some() {
                    session_history_window(event_log, before_seq, limit, None)
                } else {
                    authoritative_session_history_window(
                        transcript_store,
                        session_id,
                        before_seq,
                        limit,
                        None,
                    )
                    .await
                    .unwrap_or_else(|| session_history_window(event_log, before_seq, limit, None))
                };
                let _ = reply.send(window);
            }
            AgentCommand::ReadActivityHistory {
                after_seq,
                max_events,
                max_bytes,
                reply,
            } => {
                let _ = reply.send(activity_history_snapshot(
                    event_log, None, after_seq, max_events, max_bytes,
                ));
            }
            AgentCommand::ReadUsageSnapshot { reply } => {
                let _ = reply.send(agent_usage_snapshot_from_log(current_start, event_log));
            }
            AgentCommand::Attach { stream, reply } => {
                let attached = attach_subscriber_with_latest_output(
                    event_log,
                    None,
                    latest_output.output(),
                    status_handle.snapshot().await.is_active(),
                    subscribers,
                    stream,
                );
                let _ = reply.send(attached);
            }
            AgentCommand::Close { reply } => {
                finish_actor_close(accepting_input, status_handle, reply).await;
                break;
            }
            AgentCommand::Compact { reply, .. } => {
                let _ = reply.send(Err("backend-native agents cannot be compacted".to_owned()));
            }
            AgentCommand::CompactIfInactive {
                accepted, reply, ..
            } => {
                let error = "backend-native agents cannot be compacted".to_owned();
                let _ = accepted.send(Err(error.clone()));
                let _ = reply.send(Err(error));
            }
            AgentCommand::ReadCompactionCapability { reply } => {
                let _ = reply.send(crate::backend::BackendCompactionCapability::default());
            }
            AgentCommand::ReadRequestedCompactionRoute { reply, .. } => {
                let _ = reply.send(Err(
                    "backend-native relay agents cannot be compacted".to_owned()
                ));
            }
            AgentCommand::RequestContextCompaction { reply, .. } => {
                let _ = reply.send(Err(
                    "backend-native relay agents cannot be compacted".to_owned()
                ));
            }
            AgentCommand::ContextCompactionFallbackPrepared { result, .. } => {
                if let Ok(prepared) = result {
                    prepared.binding.backend.shutdown().await;
                }
            }
            AgentCommand::ContextCompactionTerminal { .. }
            | AgentCommand::RetryContextCompaction { .. }
            | AgentCommand::ContextCompactionBarrierExpired { .. } => {}
            AgentCommand::ReleaseCompaction { reply } => {
                let _ = reply.send(());
            }
            AgentCommand::SendInput(_) => {
                let payload = relay_input_rejected_payload(&current_start.agent_id);
                append_event(
                    canonical_stream,
                    event_log,
                    subscribers,
                    FrameKind::AgentError,
                    &payload,
                )
                .await;
            }
            AgentCommand::DeliverMessage { reply, .. } => {
                // Parked relay: terminal is the more actionable of the two
                // reasons, since resume/fork applies and "does not accept
                // direct input" would read as a routing mistake.
                let _ = reply.send(Err(DELIVERY_REJECTED_TERMINAL.to_owned()));
            }
            AgentCommand::Interrupt { reply } => {
                let payload = relay_input_rejected_payload(&current_start.agent_id);
                append_event(
                    canonical_stream,
                    event_log,
                    subscribers,
                    FrameKind::AgentError,
                    &payload,
                )
                .await;
                let _ = reply.send(InterruptOutcome::Rejected);
            }
            #[cfg(feature = "test-support")]
            AgentCommand::ForceBackendShutdownForConformance { reply } => {
                let _ = reply.send(false);
            }
            #[cfg(feature = "test-support")]
            AgentCommand::ReadMockControl { reply } => {
                let _ = reply.send(None);
            }
        }
    }
}

async fn apply_generated_agent_name(
    context: AgentNameChangeContext<'_>,
    result: Result<String, String>,
) -> bool {
    let name = match result {
        Ok(name) => name,
        Err(error) => {
            tracing::warn!(
                agent_id = %context.current_start.agent_id,
                error = %error,
                "automatic agent name generation failed; retaining fallback name"
            );
            return false;
        }
    };
    let trimmed = name.trim();
    if trimmed.is_empty() {
        tracing::warn!(
            agent_id = %context.current_start.agent_id,
            "automatic agent name generation returned an empty name; retaining fallback name"
        );
        return false;
    }

    let applied = if let Some(session_id) = context.session_id {
        match context
            .session_store
            .lock()
            .await
            .set_generated_alias_if_no_user_alias(session_id, trimmed.to_owned())
        {
            Ok(applied) => applied,
            Err(error) => {
                let payload = AgentErrorPayload {
                    agent_id: context.current_start.agent_id.clone(),
                    code: AgentErrorCode::Internal,
                    message: format!("failed to persist generated agent name: {error}"),
                    fatal: false,
                };
                append_event(
                    &format!("/agent/{}", context.current_start.agent_id),
                    context.event_log,
                    context.subscribers,
                    FrameKind::AgentError,
                    &payload,
                )
                .await;
                return false;
            }
        }
    } else if context
        .pending_alias
        .as_ref()
        .is_some_and(|alias| alias.persistence == InitialAgentAliasPersistence::User)
    {
        false
    } else {
        *context.pending_alias = Some(InitialAgentAlias {
            name: trimmed.to_owned(),
            persistence: InitialAgentAliasPersistence::GeneratedIfNoUserAlias,
        });
        true
    };
    if !applied {
        return false;
    }
    if context.current_start.name == trimmed {
        return true;
    }

    context.current_start.name = trimmed.to_owned();
    overwrite_agent_start_payload(context.event_log, context.current_start);
    let _ = context.start_tx.send_replace(context.current_start.clone());
    let payload = AgentRenamedPayload {
        agent_id: context.current_start.agent_id.clone(),
        name: context.current_start.name.clone(),
    };
    broadcast_live_event(context.subscribers, FrameKind::AgentRenamed, &payload).await;
    true
}

async fn apply_agent_name_change(
    context: AgentNameChangeContext<'_>,
    name: String,
    persistence: InitialAgentAliasPersistence,
) -> bool {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return false;
    }

    if let Some(session_id) = context.session_id {
        let persist_result = {
            let store = context.session_store.lock().await;
            match persistence {
                InitialAgentAliasPersistence::User => store
                    .set_user_alias(session_id, trimmed.to_string())
                    .map(|()| true),
                InitialAgentAliasPersistence::GeneratedIfNoUserAlias => {
                    store.set_generated_alias_if_no_user_alias(session_id, trimmed.to_string())
                }
            }
        };
        match persist_result {
            Ok(true) => {}
            // A user alias already exists (or the session is unknown); a
            // generated name never overrides it.
            Ok(false) => return false,
            Err(err) => {
                tracing::error!(
                    "failed to persist renamed agent {}: {}",
                    context.current_start.agent_id,
                    err
                );
                let payload = AgentErrorPayload {
                    agent_id: context.current_start.agent_id.clone(),
                    code: AgentErrorCode::Internal,
                    message: format!("failed to persist agent name: {err}"),
                    fatal: false,
                };
                broadcast_live_event(context.subscribers, FrameKind::AgentError, &payload).await;
                return false;
            }
        }
    } else {
        // No session yet: stage the alias. A generated name must not clobber
        // a user rename staged while the generator was running.
        if persistence == InitialAgentAliasPersistence::GeneratedIfNoUserAlias
            && matches!(
                context.pending_alias,
                Some(InitialAgentAlias {
                    persistence: InitialAgentAliasPersistence::User,
                    ..
                })
            )
        {
            return false;
        }
        *context.pending_alias = Some(InitialAgentAlias {
            name: trimmed.to_string(),
            persistence,
        });
    }

    if context.current_start.name == trimmed {
        return true;
    }

    context.current_start.name = trimmed.to_string();
    overwrite_agent_start_payload(context.event_log, context.current_start);
    // Keep the handle's snapshot view in sync so `AgentHandle::snapshot()`
    // reflects the rename without a round-trip to the actor.
    let _ = context.start_tx.send_replace(context.current_start.clone());

    let payload = AgentRenamedPayload {
        agent_id: context.current_start.agent_id.clone(),
        name: context.current_start.name.clone(),
    };
    broadcast_live_event(context.subscribers, FrameKind::AgentRenamed, &payload).await;
    true
}

fn overwrite_agent_start_payload(event_log: &mut [Envelope], current_start: &AgentStartPayload) {
    let Some(first) = event_log.first_mut() else {
        panic!("agent replay log is empty; AgentStart must always be present");
    };
    assert_eq!(
        first.kind,
        FrameKind::AgentStart,
        "agent replay log must begin with AgentStart"
    );
    first.payload = serde_json::to_value(current_start)
        .expect("failed to serialize updated AgentStart payload");
}

fn queued_message_to_send_payload(entry: QueuedMessageEntry) -> SendMessagePayload {
    SendMessagePayload {
        message: entry.message,
        images: (!entry.images.is_empty()).then_some(entry.images),
        origin: entry.origin,
        tool_response: None,
    }
}

fn internal_compaction_input(message: String) -> SendMessagePayload {
    SendMessagePayload {
        message,
        images: None,
        origin: None,
        tool_response: None,
    }
}

fn compaction_method_for_capability(
    capability: &crate::backend::BackendCompactionCapability,
) -> Option<CompactionMethod> {
    match &capability.availability {
        crate::backend::BackendCompactionAvailability::Native { mechanism } => {
            Some((*mechanism).into())
        }
        crate::backend::BackendCompactionAvailability::AutomaticOnly { .. }
        | crate::backend::BackendCompactionAvailability::Unavailable { .. } => {
            Some(CompactionMethod::InlineFallback)
        }
        crate::backend::BackendCompactionAvailability::Unknown { .. } => None,
    }
}

fn resolved_compaction_terminal_method(
    terminal_method: Option<CompactionMethod>,
    flight_method: Option<CompactionMethod>,
) -> Option<CompactionMethod> {
    terminal_method.or(flight_method)
}

/// Rebuild a queue entry from a payload the backend handed back with
/// `SendOutcome::Busy`, so it can be requeued at the front.
fn queued_entry_from_send_payload(payload: SendMessagePayload) -> QueuedMessageEntry {
    QueuedMessageEntry {
        id: QueuedMessageId(Uuid::new_v4().to_string()),
        message: payload.message,
        images: payload.images.unwrap_or_default(),
        origin: payload.origin,
    }
}

async fn notify_review_bundle_consumed(
    review_registry: &ReviewRegistryHandle,
    review_id: protocol::ReviewId,
    target_agent_id: &AgentId,
) {
    tracing::debug!(
        review_id = %review_id,
        target_agent_id = %target_agent_id,
        "notifying review bundle consumed"
    );
    match review_registry
        .bundle_consumed(review_id.clone(), target_agent_id.clone(), now_ms())
        .await
    {
        Ok(()) => {
            tracing::info!(
                review_id = %review_id,
                target_agent_id = %target_agent_id,
                "notified review bundle consumed"
            );
        }
        Err(error) => {
            tracing::warn!(
                review_id = %review_id,
                target_agent_id = %target_agent_id,
                error_len = error.len(),
                "failed to notify review bundle consumed"
            );
            let message = format!(
                "failed to mark review bundle consumed by agent {}: {}",
                target_agent_id, error
            );
            if let Err(report_error) = review_registry
                .internal_error(review_id.clone(), message, ReviewErrorContext::Submit)
                .await
            {
                tracing::warn!(
                    review_id = %review_id,
                    target_agent_id = %target_agent_id,
                    error_len = report_error.len(),
                    "failed to surface review bundle consumption error"
                );
            }
        }
    }
}

async fn emit_unknown_queued_message_error(
    canonical_stream: &str,
    event_log: &mut Vec<Envelope>,
    subscribers: &mut Vec<Stream>,
    agent_id: &AgentId,
    queued_message_id: &QueuedMessageId,
) {
    let payload = AgentErrorPayload {
        agent_id: agent_id.clone(),
        code: AgentErrorCode::Internal,
        message: format!("unknown queued message id {}", queued_message_id),
        fatal: false,
    };
    append_event(
        canonical_stream,
        event_log,
        subscribers,
        FrameKind::AgentError,
        &payload,
    )
    .await;
}

async fn persist_agent_session(
    session_store: &Arc<Mutex<SessionStore>>,
    session_id: &SessionId,
    parent_session_id: Option<SessionId>,
    current_start: &AgentStartPayload,
    current_session_settings: &SessionSettingsValues,
    resolved_spawn_config: &customization::ResolvedSpawnConfig,
    pending_alias: &mut Option<InitialAgentAlias>,
) -> Result<(), String> {
    let session = BackendSession {
        id: session_id.clone(),
        backend_kind: current_start.backend_kind,
        workspace_roots: current_start.workspace_roots.clone(),
        title: None,
        token_count: None,
        created_at_ms: Some(current_start.created_at_ms),
        updated_at_ms: Some(current_start.created_at_ms),
        resumable: current_start.origin != AgentOrigin::BackendNative
            && backend_session_is_resumable(
                current_start.backend_kind,
                session_id,
                &current_start.workspace_roots,
                resolved_spawn_config,
            ),
    };

    {
        let store = session_store.lock().await;
        store.upsert_backend_session(
            &session,
            parent_session_id,
            current_start.project_id.clone(),
            current_start.custom_agent_id.clone(),
            current_start.launch_profile_id.clone(),
        )?;
        store.set_access_mode(session_id, resolved_spawn_config.access_mode)?;
        store.set_session_settings(session_id, current_session_settings.clone())?;
        if let Some(alias) = pending_alias.take() {
            match alias.persistence {
                InitialAgentAliasPersistence::GeneratedIfNoUserAlias => {
                    let _ = store.set_generated_alias_if_no_user_alias(session_id, alias.name)?;
                }
                InitialAgentAliasPersistence::User => {
                    store.set_user_alias(session_id, alias.name)?;
                }
            }
        }
    }

    Ok(())
}

fn backend_session_is_resumable(
    backend_kind: BackendKind,
    session_id: &SessionId,
    workspace_roots: &[String],
    resolved_spawn_config: &customization::ResolvedSpawnConfig,
) -> bool {
    match backend_kind {
        BackendKind::Antigravity => is_antigravity_native_session_id(session_id),
        BackendKind::Hermes => crate::backend::hermes::session_is_resumable_for_workspace_roots(
            workspace_roots,
            resolved_spawn_config,
        ),
        BackendKind::Tycode | BackendKind::Acp | BackendKind::Claude | BackendKind::Codex => true,
    }
}

fn interrupted_tool_completion(completion: &ToolExecutionCompletedData) -> bool {
    const CLAUDE_MISSING_TOOL_RESULT: &str = "history did not contain a tool_result";

    matches!(
        &completion.outcome,
        ToolExecutionOutcome::Failed { message, details, .. }
            if message.contains(CLAUDE_MISSING_TOOL_RESULT)
                || details.as_deref().is_some_and(|details| {
                    details.contains(CLAUDE_MISSING_TOOL_RESULT)
                })
    )
}

fn tool_completion_error(completion: &ToolExecutionCompletedData) -> Option<String> {
    match &completion.outcome {
        ToolExecutionOutcome::Succeeded { .. } => None,
        ToolExecutionOutcome::Failed { message, .. }
        | ToolExecutionOutcome::Cancelled { message } => Some(message.clone()),
    }
}

async fn append_event<T: serde::Serialize>(
    canonical_stream: &str,
    event_log: &mut Vec<Envelope>,
    subscribers: &mut Vec<Stream>,
    kind: FrameKind,
    payload: &T,
) {
    let event = replay_envelope(canonical_stream, event_log.len() as u64, kind, payload);
    event_log.push(event.clone());
    broadcast_event(subscribers, &event);
}

async fn append_chat_event(
    canonical_stream: &str,
    event_log: &mut Vec<Envelope>,
    subscribers: &mut Vec<Stream>,
    replay_state: &mut AgentReplayState,
    event: &ChatEvent,
) {
    append_chat_event_with_transcript_metadata(
        canonical_stream,
        event_log,
        subscribers,
        replay_state,
        None,
        event,
    )
    .await;
}

async fn append_backend_chat_event(
    canonical_stream: &str,
    event_log: &mut Vec<Envelope>,
    subscribers: &mut Vec<Stream>,
    replay_state: &mut AgentReplayState,
    backend_kind: BackendKind,
    events: &EventStream,
    event: &ChatEvent,
) {
    append_chat_event_with_transcript_metadata(
        canonical_stream,
        event_log,
        subscribers,
        replay_state,
        Some((backend_kind, events)),
        event,
    )
    .await;
}

async fn append_chat_event_with_transcript_metadata(
    canonical_stream: &str,
    event_log: &mut Vec<Envelope>,
    subscribers: &mut Vec<Stream>,
    replay_state: &mut AgentReplayState,
    transcript_metadata: Option<(BackendKind, &EventStream)>,
    event: &ChatEvent,
) {
    let replay_len_before = event_log.len();
    record_chat_event_for_replay(canonical_stream, event_log, replay_state, event);
    if event_log.len() == replay_len_before
        && replay_state.active_stream.is_none()
        && matches!(event, ChatEvent::ToolProgress(_))
    {
        // The bounded in-memory replay log replaces progress snapshots in
        // place. The durable transcript is append-only, so persist the new
        // state as its own revision rather than mistaking an unchanged vector
        // length for "nothing happened". A revision intentionally has no
        // provider identity: several states of one tool share the provider's
        // tool id and must not deduplicate each other.
        let revision = replay_envelope(
            canonical_stream,
            event_log.len() as u64,
            FrameKind::ChatEvent,
            event,
        );
        journal_new_replay_records(
            canonical_stream,
            std::slice::from_ref(&revision),
            0,
            HashMap::new(),
        )
        .await;
    } else {
        let provider_identities =
            transcript_provider_identities(event_log, replay_len_before, transcript_metadata);
        journal_new_replay_records(
            canonical_stream,
            event_log,
            replay_len_before,
            provider_identities,
        )
        .await;
    }
    broadcast_live_event(subscribers, FrameKind::ChatEvent, event).await;
}

fn transcript_provider_identities(
    event_log: &[Envelope],
    start: usize,
    transcript_metadata: Option<(BackendKind, &EventStream)>,
) -> HashMap<u64, crate::store::transcript::ProviderEventIdentity> {
    let Some((backend_kind, events)) = transcript_metadata else {
        return HashMap::new();
    };
    event_log[start..]
        .iter()
        .filter(|envelope| envelope.kind == FrameKind::ChatEvent)
        .filter_map(|envelope| {
            let event = envelope.parse_payload::<ChatEvent>().ok()?;
            let metadata = events.transcript_metadata(&event);
            match (metadata.provider_session_id, metadata.provider_event_id) {
                (Some(provider_session_id), Some(event_id)) => Some((
                    envelope.seq,
                    crate::store::transcript::ProviderEventIdentity {
                        backend: format!("{backend_kind:?}").to_ascii_lowercase(),
                        provider_session_id: provider_session_id.0,
                        event_id,
                    },
                )),
                _ => None,
            }
        })
        .collect()
}

#[derive(Clone)]
struct RegisteredTranscriptSession {
    session_id: SessionId,
    store: TranscriptStore,
}

type TranscriptSessionRegistry = std::sync::Mutex<HashMap<String, RegisteredTranscriptSession>>;

fn transcript_session_registry() -> &'static TranscriptSessionRegistry {
    static REGISTRY: std::sync::OnceLock<TranscriptSessionRegistry> = std::sync::OnceLock::new();
    REGISTRY.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

fn register_transcript_session(
    canonical_stream: &str,
    session_id: &SessionId,
    store: &TranscriptStore,
) {
    transcript_session_registry()
        .lock()
        .expect("transcript session registry poisoned")
        .insert(
            canonical_stream.to_owned(),
            RegisteredTranscriptSession {
                session_id: session_id.clone(),
                store: store.clone(),
            },
        );
}

async fn seed_fork_transcript_history(
    store: &TranscriptStore,
    source_session_id: &SessionId,
    fork_session_id: &SessionId,
    canonical_stream: &str,
    event_log: &mut Vec<Envelope>,
) -> Result<(), String> {
    if !store.actor_io_enabled() {
        return Ok(());
    }
    let store_for_load = store.clone();
    let source_for_load = source_session_id.clone();
    let source_records = tokio::task::spawn_blocking(move || store_for_load.load(&source_for_load))
        .await
        .map_err(|error| format!("fork transcript load task failed: {error}"))??;
    let mut fork_records = Vec::new();
    for source in source_records.into_iter().filter(|record| {
        matches!(
            record.visibility,
            crate::store::transcript::TranscriptVisibility::Visible
                | crate::store::transcript::TranscriptVisibility::TimelineMarker
        )
    }) {
        let sequence = event_log.len() as u64;
        event_log.push(replay_envelope(
            canonical_stream,
            sequence,
            FrameKind::ChatEvent,
            &source.event,
        ));
        fork_records.push(crate::store::transcript::TranscriptRecord {
            logical_session_id: fork_session_id.clone(),
            sequence,
            event_id: format!("fork:{}:{}", source_session_id.0, source.event_id),
            visibility: source.visibility,
            provider_identity: source.provider_identity,
            event: source.event,
            timestamp_ms: source.timestamp_ms,
        });
    }
    let store_for_write = store.clone();
    let fork_for_write = fork_session_id.clone();
    tokio::task::spawn_blocking(move || {
        for record in &fork_records {
            store_for_write.append_import_if_missing(record)?;
        }
        store_for_write.mark_authoritative(&fork_for_write)
    })
    .await
    .map_err(|error| format!("fork transcript write task failed: {error}"))?
}

async fn seed_existing_transcript_history(
    store: &TranscriptStore,
    session_id: &SessionId,
    canonical_stream: &str,
    event_log: &mut Vec<Envelope>,
) -> Result<(), String> {
    if !store.actor_io_enabled() {
        return Ok(());
    }
    let store_for_load = store.clone();
    let session_for_load = session_id.clone();
    let records = tokio::task::spawn_blocking(move || store_for_load.load(&session_for_load))
        .await
        .map_err(|error| format!("resume transcript load task failed: {error}"))??;
    for record in records.into_iter().filter(|record| {
        matches!(
            record.visibility,
            crate::store::transcript::TranscriptVisibility::Visible
                | crate::store::transcript::TranscriptVisibility::TimelineMarker
        )
    }) {
        event_log.push(replay_envelope(
            canonical_stream,
            event_log.len() as u64,
            FrameKind::ChatEvent,
            &record.event,
        ));
    }
    Ok(())
}

async fn load_authoritative_completed_tool_call_ids(
    store: &TranscriptStore,
    session_id: &SessionId,
) -> HashSet<String> {
    if !store.actor_io_enabled() {
        return HashSet::new();
    }
    let store = store.clone();
    let session_id = session_id.clone();
    let records = match tokio::task::spawn_blocking(move || store.load(&session_id)).await {
        Ok(Ok(records)) => records,
        Ok(Err(error)) => {
            tracing::warn!(%error, "failed to load authoritative transcript dedupe state");
            return HashSet::new();
        }
        Err(error) => {
            tracing::warn!(%error, "authoritative transcript dedupe task failed");
            return HashSet::new();
        }
    };
    let mut completed_tool_call_ids = HashSet::new();
    for record in records {
        if let ChatEvent::ToolExecutionCompleted(completion) = &record.event {
            completed_tool_call_ids.insert(completion.tool_call_id.clone());
        }
    }
    completed_tool_call_ids
}

async fn journal_new_replay_records(
    canonical_stream: &str,
    event_log: &[Envelope],
    start: usize,
    provider_identities: HashMap<u64, crate::store::transcript::ProviderEventIdentity>,
) {
    let Some(registered) = transcript_session_registry()
        .lock()
        .expect("transcript session registry poisoned")
        .get(canonical_stream)
        .cloned()
    else {
        return;
    };
    let session_id = registered.session_id;
    let store = registered.store;
    if !store.actor_io_enabled() {
        return;
    }
    let records = event_log[start..]
        .iter()
        .filter(|envelope| envelope.kind == FrameKind::ChatEvent)
        .filter_map(|envelope| {
            let event = envelope.parse_payload::<ChatEvent>().ok()?;
            let (event_id, visibility) = match &event {
                ChatEvent::ContextCompaction(marker) => (
                    marker.marker_id.0.clone(),
                    crate::store::transcript::TranscriptVisibility::TimelineMarker,
                ),
                _ => (
                    uuid::Uuid::new_v4().to_string(),
                    crate::store::transcript::TranscriptVisibility::Visible,
                ),
            };
            Some(crate::store::transcript::TranscriptRecord {
                logical_session_id: session_id.clone(),
                sequence: envelope.seq,
                event_id,
                visibility,
                provider_identity: provider_identities.get(&envelope.seq).cloned(),
                event,
                timestamp_ms: now_ms(),
            })
        })
        .collect::<Vec<_>>();
    if records.is_empty() {
        return;
    }
    let persistence_session_id = session_id.clone();
    let persistence = tokio::task::spawn_blocking(move || {
        let mut next_sequence = store
            .load(&persistence_session_id)?
            .into_iter()
            .map(|record| record.sequence)
            .max()
            .map_or(0, |sequence| sequence.saturating_add(1));
        for mut record in records {
            record.sequence = next_sequence;
            if store.append_import_if_missing(&record)? {
                next_sequence = next_sequence.saturating_add(1);
            }
        }
        Ok::<(), String>(())
    })
    .await;
    match persistence {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            tracing::warn!(
                session_id = %session_id,
                %error,
                "failed to append materialized transcript record"
            );
        }
        Err(error) => {
            tracing::warn!(
                session_id = %session_id,
                %error,
                "transcript persistence task failed"
            );
        }
    }
}

async fn mark_transcript_authoritative(store: &TranscriptStore, session_id: &SessionId) {
    if !store.actor_io_enabled() {
        return;
    }
    let session_id = session_id.clone();
    let store = store.clone();
    let persisted = tokio::task::spawn_blocking({
        let session_id = session_id.clone();
        move || store.mark_authoritative(&session_id)
    })
    .await;
    match persisted {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            tracing::warn!(
                session_id = %session_id,
                %error,
                "failed to mark canonical transcript authoritative"
            );
        }
        Err(error) => {
            tracing::warn!(
                session_id = %session_id,
                %error,
                "transcript authority persistence task failed"
            );
        }
    }
}

async fn upsert_activity_stats_snapshot(
    canonical_stream: &str,
    event_log: &mut Vec<Envelope>,
    subscribers: &mut Vec<Stream>,
    agent_id: &AgentId,
    stats: AgentActivityStats,
) {
    let payload = AgentActivityStatsPayload {
        agent_id: agent_id.clone(),
        stats,
    };
    let value =
        serde_json::to_value(&payload).expect("failed to serialize AgentActivityStats payload");

    if let Some(snapshot) = event_log
        .iter_mut()
        .find(|event| event.kind == FrameKind::AgentActivityStats)
    {
        snapshot.payload = value.clone();
    } else {
        event_log.push(Envelope {
            stream: protocol::StreamPath(canonical_stream.to_owned()),
            kind: FrameKind::AgentActivityStats,
            seq: event_log.len() as u64,
            payload: value.clone(),
        });
    }

    broadcast_live_event(subscribers, FrameKind::AgentActivityStats, &payload).await;
}

fn spawn_resume_replay_barrier_task(
    tx: mpsc::UnboundedSender<AgentCommand>,
    barrier_rx: oneshot::Receiver<()>,
    agent_id: AgentId,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let result = match tokio::time::timeout(RESUME_REPLAY_BARRIER_TIMEOUT, barrier_rx).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err("agent backend ended before resume replay completed".to_owned()),
            Err(_) => Err(format!(
                "timed out after {}s waiting for resume replay to complete",
                RESUME_REPLAY_BARRIER_TIMEOUT.as_secs()
            )),
        };
        if result.is_err() {
            tracing::warn!(agent_id = %agent_id, "resume replay barrier failed");
        }
        let _ = tx.send(AgentCommand::ResumeReplayBarrier { result });
    })
}

fn abort_resume_replay_barrier_task(task: &mut Option<tokio::task::JoinHandle<()>>) {
    if let Some(task) = task.take() {
        task.abort();
    }
}

async fn flush_pending_agent_attaches(
    event_log: &[Envelope],
    replay_state: Option<&AgentReplayState>,
    latest_output: &mut AgentControlLatestOutput,
    subscribers: &mut Vec<Stream>,
    pending_attaches: &mut Vec<(Stream, oneshot::Sender<bool>)>,
    status_handle: &registry::AgentStatusHandle,
) {
    let output = current_latest_output(latest_output, event_log)
        .expect("typed agent replay log must project latest output");
    let turn_active = status_handle.snapshot().await.is_active();
    for (stream, reply) in std::mem::take(pending_attaches) {
        let attached = attach_subscriber_with_latest_output(
            event_log,
            replay_state,
            &output,
            turn_active,
            subscribers,
            stream,
        );
        let _ = reply.send(attached);
    }
}

async fn send_initial_follow_up_or_park(
    input: SendMessagePayload,
    context: InitialFollowUpContext<'_>,
) -> bool {
    tracing::info!(
        agent_id = %context.current_start.agent_id,
        "dispatching initial resumed-session follow-up"
    );
    *context.in_turn = true;
    *context.idle_transition_armed = false;
    match context
        .backend
        .as_ref()
        .expect("backend must exist after successful startup")
        .send_with_outcome(AgentInput::SendMessage(input))
        .await
    {
        SendOutcome::Accepted => {
            tracing::info!(
                agent_id = %context.current_start.agent_id,
                "initial resumed-session follow-up accepted by backend"
            );
            mark_agent_turn_active(context.status_handle).await;
            return true;
        }
        SendOutcome::Busy(input) => {
            if let AgentInput::SendMessage(payload) = input {
                tracing::info!(
                    agent_id = %context.current_start.agent_id,
                    "backend busy with a self-started turn; initial follow-up queued at front"
                );
                let sequence = *context.next_queue_sequence;
                *context.next_queue_sequence = (*context.next_queue_sequence).saturating_add(1);
                context.queue.push_front(SequencedQueuedMessage {
                    sequence,
                    entry: queued_entry_from_send_payload(payload),
                });
                update_queued_messages_snapshot(
                    context.canonical_stream,
                    context.event_log,
                    context.subscribers,
                    context.queue,
                    context.session_store,
                )
                .await;
            } else {
                tracing::error!(
                    agent_id = %context.current_start.agent_id,
                    "backend handed back a non-message input as Busy"
                );
            }
            return true;
        }
        SendOutcome::Closed => {}
    }

    let payload = AgentErrorPayload {
        agent_id: context.current_start.agent_id.clone(),
        code: AgentErrorCode::Internal,
        message: "agent backend closed".to_owned(),
        fatal: true,
    };
    enter_terminal_failure(
        TerminalFailureContext {
            accepting_input: context.accepting_input,
            status_handle: context.status_handle,
            canonical_stream: context.canonical_stream,
            event_log: context.event_log,
            replay_state: context.replay_state,
            subscribers: context.subscribers,
            queue: context.queue,
            session_store: context.session_store,
            compaction: None,
        },
        &payload,
    )
    .await;
    park_terminal_agent(
        context.session_store,
        context.transcript_store,
        context.current_session_id,
        context.pending_alias,
        context.current_start,
        context.start_tx,
        context.event_log,
        context.latest_output,
        context.subscribers,
        context.pending_inputs,
        context.rx,
    )
    .await;
    false
}

async fn terminalize_closed_queue_dispatch(context: QueueDispatchTerminalContext<'_>) {
    let payload = AgentErrorPayload {
        agent_id: context.current_start.agent_id.clone(),
        code: AgentErrorCode::BackendFailed,
        message: "agent backend closed while dispatching a queued message".to_owned(),
        fatal: true,
    };
    terminalize_live_activity(
        LiveActivityTerminalContext {
            canonical_stream: context.canonical_stream,
            event_log: context.event_log,
            replay_state: context.replay_state,
            subscribers: context.subscribers,
            open_tool_call_ids: context.open_tool_call_ids,
            pending_tool_response_ids: context.pending_tool_response_ids,
            active_agent_await_ids: context.active_agent_await_ids,
        },
        LiveActivityTerminalStatus::Failed,
        &payload.message,
    )
    .await;
    enter_terminal_failure(
        TerminalFailureContext {
            accepting_input: context.accepting_input,
            status_handle: context.status_handle,
            canonical_stream: context.canonical_stream,
            event_log: context.event_log,
            replay_state: context.replay_state,
            subscribers: context.subscribers,
            queue: context.queue,
            session_store: context.session_store,
            compaction: context.current_session_id.map(|session_id| {
                TerminalCompactionFailureContext {
                    flight: context.context_compaction,
                    session_store: context.session_store,
                    session_id,
                    start: &*context.current_start,
                    activity_stats: context.activity_stats,
                }
            }),
        },
        &payload,
    )
    .await;
    park_terminal_agent(
        context.session_store,
        context.transcript_store,
        context.current_session_id,
        context.pending_alias,
        context.current_start,
        context.start_tx,
        context.event_log,
        context.latest_output,
        context.subscribers,
        context.pending_inputs,
        context.rx,
    )
    .await;
}

async fn mark_agent_turn_active(status_handle: &registry::AgentStatusHandle) {
    status_handle
        .update(|status| {
            status.is_thinking = true;
            status.turn_completed = false;
            status.last_error = None;
            status.activity_counter = status.activity_counter.saturating_add(1);
            // A live turn is the thing a restored transcript was missing, and
            // it is where the supervisor's stall clock starts.
            status.restored_without_live_turn = false;
            status.turn_started_at = Some(Instant::now());
        })
        .await;
}

fn backend_turn_visibly_busy(backend_typing: bool, pending_tool_responses: usize) -> bool {
    backend_typing || pending_tool_responses > 0
}

fn record_agent_started(status: &mut registry::AgentStatus, is_resume: bool) {
    status.started = true;
    if is_resume {
        status.is_thinking = false;
        status.turn_completed = true;
        // Replayed history is not work this agent just did. Cleared by the
        // first live turn (see `mark_agent_turn_active`).
        status.restored_without_live_turn = true;
    }
    status.last_error = None;
    status.activity_counter = status.activity_counter.saturating_add(1);
}

async fn publish_resumed_agent_idle(
    status_handle: &registry::AgentStatusHandle,
    canonical_stream: &str,
    event_log: &mut Vec<Envelope>,
    subscribers: &mut Vec<Stream>,
    replay_state: &mut AgentReplayState,
) {
    status_handle
        .update(|status| {
            status.is_thinking = false;
            status.turn_completed = true;
            status.activity_counter = status.activity_counter.saturating_add(1);
        })
        .await;
    append_chat_event(
        canonical_stream,
        event_log,
        subscribers,
        replay_state,
        &ChatEvent::TypingStatusChanged(false),
    )
    .await;
    replay_state.resume_history_settled_idle = true;
}

/// Ingest a backend event that arrived while the resume-replay gate is still
/// pending: update activity stats and record it into the event log via the
/// replay state, but never broadcast it to subscribers as a live event.
///
/// Shared by the gated `events.recv()` branch and the drain that runs when the
/// resume-replay barrier fires. The resume loop's `select!` is unbiased, so the
/// barrier command can be handled while replay events are still buffered on the
/// backend stream; routing both paths through here guarantees a buffered replay
/// event can never leak onto the live broadcast just because the gate closed
/// first.
#[allow(clippy::too_many_arguments)]
async fn ingest_gated_replay_event(
    event: &mut ChatEvent,
    canonical_stream: &str,
    agent_id: &AgentId,
    event_log: &mut Vec<Envelope>,
    subscribers: &mut Vec<Stream>,
    replay_state: &mut AgentReplayState,
    activity_stats: &mut AgentActivityStatsTracker,
    active_stream_text: &mut String,
    activity_event_seq: &mut u64,
) {
    project_legacy_native_collaboration_event(event);
    match &*event {
        ChatEvent::StreamStart(_) => active_stream_text.clear(),
        ChatEvent::StreamDelta(delta) => active_stream_text.push_str(&delta.text),
        _ => {}
    }
    let source_seq = *activity_event_seq;
    *activity_event_seq = activity_event_seq.saturating_add(1);
    if activity_stats.observe_chat_event(event, source_seq, active_stream_text) {
        upsert_activity_stats_snapshot(
            canonical_stream,
            event_log,
            subscribers,
            agent_id,
            activity_stats.snapshot(),
        )
        .await;
    }
    if matches!(&*event, ChatEvent::StreamEnd(_)) {
        active_stream_text.clear();
    }
    record_chat_event_for_replay(canonical_stream, event_log, replay_state, &*event);
}

fn record_chat_event_for_replay(
    canonical_stream: &str,
    event_log: &mut Vec<Envelope>,
    replay_state: &mut AgentReplayState,
    event: &ChatEvent,
) {
    match event {
        ChatEvent::StreamStart(start) => {
            if replay_state.active_stream.take().is_some() {
                tracing::warn!("replacing an unterminated response with a new StreamStart");
            }
            replay_state.active_stream = Some(ReplayActiveStream {
                start: start.clone(),
                text: String::new(),
                reasoning: String::new(),
                tool_events: Vec::new(),
            });
        }
        ChatEvent::StreamDelta(delta) => {
            if let Some(active) = replay_state.active_stream.as_mut() {
                active.text.push_str(&delta.text);
            } else {
                tracing::warn!("dropping StreamDelta without an active response");
            }
        }
        ChatEvent::StreamReasoningDelta(delta) => {
            if let Some(active) = replay_state.active_stream.as_mut() {
                active.reasoning.push_str(&delta.text);
            } else {
                tracing::warn!("dropping StreamReasoningDelta without an active response");
            }
        }
        ChatEvent::StreamEnd(data) => {
            let Some(stream) = replay_state.active_stream.take() else {
                tracing::warn!("recording StreamEnd without a preceding StreamStart");
                push_chat_event_to_replay_log(canonical_stream, event_log, event);
                return;
            };
            push_chat_event_to_replay_log(
                canonical_stream,
                event_log,
                &ChatEvent::StreamStart(stream.start),
            );
            if !stream.reasoning.is_empty() {
                push_chat_event_to_replay_log(
                    canonical_stream,
                    event_log,
                    &ChatEvent::StreamReasoningDelta(StreamTextDeltaData {
                        text: stream.reasoning,
                    }),
                );
            }
            if !stream.text.is_empty() {
                push_chat_event_to_replay_log(
                    canonical_stream,
                    event_log,
                    &ChatEvent::StreamDelta(StreamTextDeltaData { text: stream.text }),
                );
            }
            for tool_event in stream.tool_events {
                if let ChatEvent::ToolProgress(progress) = &tool_event {
                    coalesce_progress_into_replay_log(
                        canonical_stream,
                        event_log,
                        replay_state,
                        progress.tool_call_id.clone(),
                        &tool_event,
                    );
                } else {
                    push_chat_event_to_replay_log(canonical_stream, event_log, &tool_event);
                }
            }
            push_chat_event_to_replay_log(
                canonical_stream,
                event_log,
                &ChatEvent::StreamEnd(data.clone()),
            );
        }
        ChatEvent::MessageMetadataUpdated(update) => {
            push_chat_event_to_replay_log(
                canonical_stream,
                event_log,
                &ChatEvent::MessageMetadataUpdated(update.clone()),
            );
        }
        ChatEvent::ToolRequest(_) => {
            if let Some(active) = replay_state.active_stream.as_mut() {
                active.tool_events.push(event.clone());
            } else {
                push_chat_event_to_replay_log(canonical_stream, event_log, event);
            }
        }
        ChatEvent::ToolExecutionCompleted(completion) => {
            replay_state
                .active_tool_progress
                .remove(&completion.tool_call_id);
            replay_state
                .active_background_progress
                .remove(&completion.tool_call_id);
            if let Some(active) = replay_state.active_stream.as_mut()
                && active.tool_events.iter().any(|buffered| {
                    matches!(
                        buffered,
                        ChatEvent::ToolRequest(request)
                            if request.tool_call_id == completion.tool_call_id
                    )
                })
            {
                active.tool_events.push(event.clone());
            } else {
                push_chat_event_to_replay_log(canonical_stream, event_log, event);
            }
        }
        ChatEvent::ToolProgress(data) => {
            let running = match &data.update {
                protocol::ToolProgressUpdate::SubAgent(state) => {
                    state.status == protocol::SubAgentProgressStatus::Running && !state.completed
                }
                protocol::ToolProgressUpdate::Workflow(state) => {
                    state.status == protocol::WorkflowRunStatus::Running
                }
                protocol::ToolProgressUpdate::AgentControl(state) => {
                    state.status == protocol::AgentControlProgressStatus::Running
                }
                protocol::ToolProgressUpdate::Other { .. } => true,
            };
            if running {
                replay_state
                    .active_tool_progress
                    .insert(data.tool_call_id.clone(), data.clone());
                if data.execution_mode == ToolExecutionMode::Background {
                    replay_state
                        .active_background_progress
                        .insert(data.tool_call_id.clone(), data.clone());
                }
            } else {
                // Both sets are otherwise only ever emptied by the tool's
                // completion, and background work reports its terminal snapshot
                // *after* that. Leaving the entry behind holds
                // `background_mutation_active` true for the rest of the session,
                // which defers every later compaction.
                replay_state.active_tool_progress.remove(&data.tool_call_id);
                replay_state
                    .active_background_progress
                    .remove(&data.tool_call_id);
            }
            if let Some(active) = replay_state.active_stream.as_mut() {
                let existing = active.tool_events.iter_mut().find(|buffered| {
                    matches!(
                        buffered,
                        ChatEvent::ToolProgress(progress)
                            if progress.tool_call_id == data.tool_call_id
                    )
                });
                if let Some(existing) = existing {
                    *existing = event.clone();
                } else {
                    active.tool_events.push(event.clone());
                }
            } else {
                coalesce_progress_into_replay_log(
                    canonical_stream,
                    event_log,
                    replay_state,
                    data.tool_call_id.clone(),
                    event,
                );
            }
        }
        ChatEvent::OperationCancelled(_) => {
            replay_state.operation_cancelled = true;
            if let Some(stream) = replay_state.active_stream.take() {
                for tool_event in stream.tool_events {
                    push_chat_event_to_replay_log(canonical_stream, event_log, &tool_event);
                }
            }
            push_chat_event_to_replay_log(canonical_stream, event_log, event);
        }
        ChatEvent::TypingStatusChanged(typing) => {
            replay_state.typing = *typing;
            if *typing {
                replay_state.operation_cancelled = false;
                replay_state.resume_history_settled_idle = false;
            } else if replay_state.active_stream.take().is_some() {
                tracing::warn!("discarding an unterminated response when the agent became idle");
            }
            push_chat_event_to_replay_log(canonical_stream, event_log, event);
        }
        ChatEvent::MessageAdded(_)
        | ChatEvent::TaskUpdate(_)
        | ChatEvent::RetryAttempt(_)
        | ChatEvent::Orchestration(_)
        | ChatEvent::ContextCompaction(_) => {
            push_chat_event_to_replay_log(canonical_stream, event_log, event);
        }
    }
}

/// Latest-wins coalescing for `ToolProgress`: at most one envelope per
/// tool_call_id is retained in the event_log, replaced in place so its
/// seq (and thus replay ordering relative to the tool's request and
/// completion) is preserved.
fn coalesce_progress_into_replay_log(
    canonical_stream: &str,
    event_log: &mut Vec<Envelope>,
    replay_state: &mut AgentReplayState,
    tool_call_id: String,
    event: &ChatEvent,
) {
    if let Some(&index) = replay_state.progress_log_index.get(&tool_call_id) {
        let seq = event_log[index].seq;
        event_log[index] = replay_envelope(canonical_stream, seq, FrameKind::ChatEvent, event);
    } else {
        replay_state
            .progress_log_index
            .insert(tool_call_id, event_log.len());
        push_chat_event_to_replay_log(canonical_stream, event_log, event);
    }
}

fn push_chat_event_to_replay_log(
    canonical_stream: &str,
    event_log: &mut Vec<Envelope>,
    event: &ChatEvent,
) {
    let envelope = replay_envelope(
        canonical_stream,
        event_log.len() as u64,
        FrameKind::ChatEvent,
        event,
    );
    event_log.push(envelope);
}

fn replay_log_latest_task_snapshot_is(
    event_log: &[Envelope],
    expected: &protocol::TaskList,
) -> bool {
    let expected = serde_json::to_value(expected).expect("serialize persisted task snapshot");
    event_log
        .iter()
        .rev()
        .filter(|envelope| envelope.kind == FrameKind::ChatEvent)
        .find_map(
            |envelope| match envelope.parse_payload::<ChatEvent>().ok()? {
                ChatEvent::TaskUpdate(tasks) => serde_json::to_value(tasks).ok(),
                _ => None,
            },
        )
        .is_some_and(|tasks| tasks == expected)
}

fn replay_envelope<T: serde::Serialize>(
    canonical_stream: &str,
    seq: u64,
    kind: FrameKind,
    payload: &T,
) -> Envelope {
    Envelope::from_payload(
        protocol::StreamPath(canonical_stream.to_owned()),
        kind,
        seq,
        payload,
    )
    .expect("failed to serialize protocol payload in agent actor")
}

fn output_events_since(
    event_log: &[Envelope],
    after_seq: Option<u64>,
    limit: usize,
) -> Vec<Envelope> {
    event_log
        .iter()
        .filter(|event| after_seq.is_none_or(|seq| event.seq > seq))
        .filter(|event| matches!(event.kind, FrameKind::ChatEvent | FrameKind::AgentError))
        .take(limit)
        .cloned()
        .collect()
}

fn current_latest_output(
    latest_output: &mut AgentControlLatestOutput,
    event_log: &[Envelope],
) -> Result<AgentControlOutput, String> {
    latest_output
        .observe_event_log(event_log)
        .map_err(|error| error.to_string())?;
    Ok(latest_output.output().clone())
}

fn activity_history_snapshot(
    event_log: &[Envelope],
    replay_state: Option<&AgentReplayState>,
    after_seq: Option<u64>,
    max_events: usize,
    max_bytes: usize,
) -> AgentActivityHistorySnapshot {
    let mut entries = Vec::new();
    for envelope in event_log {
        if after_seq.is_some_and(|seq| envelope.seq <= seq) {
            continue;
        }
        match envelope.kind {
            FrameKind::ChatEvent => {
                if let Ok(event) = serde_json::from_value::<ChatEvent>(envelope.payload.clone())
                    && let Some(rendered) = render_activity_chat_event(&event)
                {
                    entries.push((envelope.seq, rendered));
                }
            }
            FrameKind::AgentError => {
                if let Ok(error) =
                    serde_json::from_value::<AgentErrorPayload>(envelope.payload.clone())
                {
                    entries.push((
                        envelope.seq,
                        cap_activity_text(&format!("Agent error: {}", error.message), 1024),
                    ));
                }
            }
            _ => {}
        }
    }

    let mut active_stream_included = false;
    if let Some(replay_state) = replay_state {
        for (index, event) in replay_state.active_stream_events().into_iter().enumerate() {
            if let Some(rendered) = render_activity_chat_event(&event) {
                active_stream_included = true;
                entries.push((event_log.len() as u64 + index as u64, rendered));
            }
        }
    }

    let max_events = max_events.max(1);
    let max_bytes = max_bytes.max(1);
    if entries.len() > max_events {
        let start = entries.len() - max_events;
        entries.drain(0..start);
    }

    while rendered_activity_entries_len(&entries) > max_bytes && entries.len() > 1 {
        entries.remove(0);
    }

    let mut rendered = String::new();
    for (seq, line) in &entries {
        if !rendered.is_empty() {
            rendered.push('\n');
        }
        rendered.push_str(&format!("[seq {seq}] {line}"));
    }

    AgentActivityHistorySnapshot {
        rendered,
        from_seq: entries.first().map(|(seq, _)| *seq),
        through_seq: entries.last().map(|(seq, _)| *seq),
        event_count: entries.len(),
        active_stream_included,
    }
}

fn agent_usage_snapshot_from_tracker(
    start: &AgentStartPayload,
    tracker: &AgentActivityStatsTracker,
) -> AgentUsageSnapshot {
    let (usage, model) = tracker.usage_snapshot();
    AgentUsageSnapshot {
        start: start.clone(),
        usage,
        model,
    }
}

fn agent_usage_snapshot_from_log(
    start: &AgentStartPayload,
    event_log: &[Envelope],
) -> AgentUsageSnapshot {
    let mut tracker = AgentActivityStatsTracker::for_backend(start.backend_kind);
    let mut active_stream_text = String::new();
    let mut saw_replayable_usage_event = false;
    let mut latest_stats = None;
    for envelope in event_log {
        match envelope.kind {
            FrameKind::ChatEvent => {
                let Ok(mut event) = serde_json::from_value::<ChatEvent>(envelope.payload.clone())
                else {
                    continue;
                };
                if chat_event_can_reconstruct_usage(&event) {
                    saw_replayable_usage_event = true;
                }
                strip_replayed_cumulative_token_usage(&mut event);
                match &event {
                    ChatEvent::StreamStart(_) => active_stream_text.clear(),
                    ChatEvent::StreamDelta(delta) => active_stream_text.push_str(&delta.text),
                    _ => {}
                }
                tracker.observe_chat_event(&mut event, envelope.seq, &active_stream_text);
                if matches!(event, ChatEvent::StreamEnd(_)) {
                    active_stream_text.clear();
                }
            }
            FrameKind::AgentActivityStats => {
                if let Ok(payload) =
                    serde_json::from_value::<AgentActivityStatsPayload>(envelope.payload.clone())
                {
                    latest_stats = Some(payload.stats);
                }
            }
            _ => {}
        }
    }
    if let Some(total_tokens) = latest_stats.as_ref().and_then(|stats| {
        stats
            .token_usage_total_only
            .filter(|total| *total >= stats.token_usage.total_tokens)
    }) {
        return AgentUsageSnapshot {
            start: start.clone(),
            usage: TaskTokenUsageScope::Known {
                usage: Box::new(TaskTokenUsageAmount::total_only(total_tokens)),
            },
            model: tracker.latest_model,
        };
    }
    if start.backend_kind == BackendKind::Codex
        && let Some(stats) = latest_stats.as_ref()
        && stats.token_usage.total_tokens > 0
    {
        return AgentUsageSnapshot {
            start: start.clone(),
            usage: TaskTokenUsageScope::Known {
                usage: Box::new(TaskTokenUsageAmount::from_token_usage(&stats.token_usage)),
            },
            model: tracker.latest_model,
        };
    }
    if saw_replayable_usage_event {
        let reported_usage_floor = latest_stats.as_ref().map(|stats| &stats.token_usage);
        let (usage, model) = tracker.usage_snapshot_with_reported_usage_floor(reported_usage_floor);
        return AgentUsageSnapshot {
            start: start.clone(),
            usage,
            model,
        };
    }

    // Legacy logs can contain only the coalesced activity snapshot. In that
    // case there is no replayable source-level usage state, so keep the old
    // stats-only reconstruction path explicit.
    let stats = latest_stats.unwrap_or_default();
    let usage = if stats.token_usage.total_tokens > 0 {
        TaskTokenUsageScope::Known {
            usage: Box::new(TaskTokenUsageAmount::from_token_usage(&stats.token_usage)),
        }
    } else {
        TaskTokenUsageScope::Unavailable {
            reason: TaskTokenUsageUnavailableReason::NoAssistantTurnCompleted,
        }
    };
    AgentUsageSnapshot {
        start: start.clone(),
        usage,
        model: None,
    }
}

fn chat_event_can_reconstruct_usage(event: &ChatEvent) -> bool {
    match event {
        ChatEvent::MessageAdded(message) | ChatEvent::StreamEnd(StreamEndData { message }) => {
            matches!(message.sender, MessageSender::Assistant { .. })
        }
        ChatEvent::MessageMetadataUpdated(update) => update.token_usage.is_some(),
        _ => false,
    }
}

fn strip_replayed_cumulative_token_usage(event: &mut ChatEvent) {
    let token_usage = match event {
        ChatEvent::MessageAdded(message) => message.token_usage.as_mut(),
        ChatEvent::StreamEnd(data) => data.message.token_usage.as_mut(),
        ChatEvent::MessageMetadataUpdated(update) => update.token_usage.as_mut(),
        _ => None,
    };
    if let Some(token_usage) = token_usage {
        token_usage.cumulative = TokenUsageScope::Unavailable {
            reason: TokenUsageUnavailableReason::BackendDidNotReport,
        };
    }
}

fn rendered_activity_entries_len(entries: &[(u64, String)]) -> usize {
    entries
        .iter()
        .map(|(seq, line)| "[seq ] ".len() + seq.to_string().len() + line.len() + 1)
        .sum()
}

fn tool_request_label(tool_type: &ToolRequestType) -> &'static str {
    match tool_type {
        ToolRequestType::ModifyFile { .. } => "modify_file",
        ToolRequestType::RunCommand { .. } => "run_command",
        ToolRequestType::ReadFiles { .. } => "read_files",
        ToolRequestType::SearchTypes { .. } => "search_types",
        ToolRequestType::GetTypeDocs { .. } => "get_type_docs",
        ToolRequestType::AskUserQuestion { .. } => "ask_user_question",
        ToolRequestType::ExitPlanMode { .. } => "exit_plan_mode",
        ToolRequestType::AgentSpawn { .. } => "agent_spawn",
        ToolRequestType::GenerateImage { .. } => "generate_image",
        ToolRequestType::WebSearch { .. } => "web_search",
        ToolRequestType::ViewImage { .. } => "view_image",
        ToolRequestType::Sleep { .. } => "sleep",
        ToolRequestType::TydeSendAgentMessage { .. } => "tyde_send_agent_message",
        ToolRequestType::TydeAwaitAgents { .. } => "tyde_await_agents",
        ToolRequestType::Other { .. } => "other",
    }
}

fn render_activity_chat_event(event: &ChatEvent) -> Option<String> {
    match event {
        ChatEvent::MessageAdded(message) => {
            let sender = match &message.sender {
                MessageSender::User => "User",
                MessageSender::System => "System",
                MessageSender::Warning => "Warning",
                MessageSender::Error => "Error",
                MessageSender::Assistant { .. } => "Assistant",
            };
            let mut parts = Vec::new();
            if !message.content.trim().is_empty() {
                parts.push(cap_activity_text(message.content.trim(), 1200));
            }
            if let Some(reasoning) = &message.reasoning
                && !reasoning.text.trim().is_empty()
            {
                parts.push(format!(
                    "reasoning: {}",
                    cap_activity_text(reasoning.text.trim(), 600)
                ));
            }
            if !message.tool_calls.is_empty() {
                let tool_names = message
                    .tool_calls
                    .iter()
                    .map(|tool| tool.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                parts.push(format!("tool calls: {tool_names}"));
            }
            (!parts.is_empty()).then(|| format!("{sender}: {}", parts.join(" | ")))
        }
        ChatEvent::StreamStart(start) => {
            Some(format!("Assistant started streaming as {}", start.agent))
        }
        ChatEvent::StreamDelta(delta) => {
            let text = delta.text.trim();
            (!text.is_empty())
                .then(|| format!("Assistant streaming: {}", cap_activity_text(text, 1200)))
        }
        ChatEvent::StreamReasoningDelta(delta) => {
            let text = delta.text.trim();
            (!text.is_empty())
                .then(|| format!("Assistant reasoning: {}", cap_activity_text(text, 800)))
        }
        ChatEvent::StreamEnd(data) => {
            let text = data.message.content.trim();
            (!text.is_empty())
                .then(|| format!("Assistant finished: {}", cap_activity_text(text, 1200)))
        }
        ChatEvent::ToolRequest(request) => Some(format!(
            "Tool requested: {} [{}]",
            tool_request_label(&request.tool_type),
            request.tool_call_id
        )),
        ChatEvent::ToolProgress(progress) => {
            Some(format!("Tool progress: {}", progress.tool_call_id))
        }
        ChatEvent::ToolExecutionCompleted(completion) => Some(format!(
            "Tool {} {}",
            completion.tool_call_id,
            match &completion.outcome {
                ToolExecutionOutcome::Succeeded { .. } => "completed",
                ToolExecutionOutcome::Failed { .. } => "failed",
                ToolExecutionOutcome::Cancelled { .. } => "was cancelled",
            }
        )),
        ChatEvent::TaskUpdate(tasks) => {
            let title = tasks.title.trim();
            if title.is_empty() {
                Some(format!(
                    "Task list updated with {} tasks",
                    tasks.tasks.len()
                ))
            } else {
                Some(format!(
                    "Task list updated: {}",
                    cap_activity_text(title, 300)
                ))
            }
        }
        ChatEvent::OperationCancelled(cancelled) => Some(format!(
            "Operation cancelled: {}",
            cap_activity_text(&cancelled.message, 500)
        )),
        ChatEvent::RetryAttempt(retry) => Some(format!(
            "Retry attempt {}/{} after error: {}",
            retry.attempt,
            retry.max_retries,
            cap_activity_text(&retry.error, 500)
        )),
        ChatEvent::TypingStatusChanged(typing) => {
            Some(format!("Agent typing status changed: {typing}"))
        }
        ChatEvent::Orchestration(event) => Some(format!(
            "Orchestration {}: {}",
            event.agent_type,
            event.payload.kind()
        )),
        ChatEvent::ContextCompaction(event) => Some(format!(
            "Context compaction {:?}: {:?} ({:?})",
            event.method, event.status, event.mutation
        )),
        ChatEvent::MessageMetadataUpdated(_) => None,
    }
}

fn cap_activity_text(text: &str, max_chars: usize) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(max_chars)
        .collect()
}

async fn update_queued_messages_snapshot(
    canonical_stream: &str,
    event_log: &mut Vec<Envelope>,
    subscribers: &mut Vec<Stream>,
    queue: &VecDeque<SequencedQueuedMessage>,
    session_store: &Arc<Mutex<SessionStore>>,
) {
    let payload = QueuedMessagesPayload {
        messages: queue.iter().map(|queued| queued.entry.clone()).collect(),
    };
    if let Some(session_id) = event_log.iter().find_map(|event| {
        (event.kind == FrameKind::AgentStart)
            .then(|| event.parse_payload::<AgentStartPayload>().ok())
            .flatten()
            .and_then(|start| start.session_id)
    }) {
        let messages = payload.messages.clone();
        if let Err(error) = session_store
            .lock()
            .await
            .update(&session_id, |record| record.queued_messages = messages)
        {
            tracing::error!(%session_id, %error, "failed to persist queued messages");
        }
    }
    let value =
        serde_json::to_value(&payload).expect("failed to serialize queued messages payload");

    if let Some(snapshot) = event_log
        .iter_mut()
        .find(|event| event.kind == FrameKind::QueuedMessages)
    {
        snapshot.payload = value.clone();
    } else {
        event_log.push(Envelope {
            stream: protocol::StreamPath(canonical_stream.to_owned()),
            kind: FrameKind::QueuedMessages,
            seq: event_log.len() as u64,
            payload: value.clone(),
        });
    }

    broadcast_live_event(subscribers, FrameKind::QueuedMessages, &payload).await;
}

struct ContextCompactionDispatchContext<'a> {
    actor_tx: &'a mpsc::UnboundedSender<AgentCommand>,
    backend: &'a dyn BackendSender,
    session_store: &'a Arc<Mutex<SessionStore>>,
    transcript_store: &'a TranscriptStore,
    session_id: &'a SessionId,
    start: &'a AgentStartPayload,
    status_handle: &'a registry::AgentStatusHandle,
    current_session_settings: &'a SessionSettingsValues,
    canonical_stream: &'a str,
    event_log: &'a mut Vec<Envelope>,
    subscribers: &'a mut Vec<Stream>,
    spawn_config: &'a BackendSpawnConfig,
    use_mock_backend: bool,
    capacity_tx: &'a HostCapacityTx,
    antigravity_conversations_dir: &'a PathBuf,
}

struct ContextCompactionDispatchReadiness<'a> {
    queue: &'a VecDeque<SequencedQueuedMessage>,
    in_turn: bool,
    replay_pending: bool,
    open_tool_call_ids: &'a HashSet<String>,
    pending_tool_response_ids: &'a HashSet<String>,
    background_mutation_active: bool,
}

struct ContextCompactionTerminalRecord {
    accepted: bool,
    mutation: CompactionMutation,
    method: Option<CompactionMethod>,
    metrics: CompactionMetrics,
    provider_session_id: Option<SessionId>,
    status: ContextCompactionTimelineStatus,
    message: Option<String>,
    trusted_post_context_tokens: Option<Option<u64>>,
}

#[allow(clippy::too_many_arguments)]
async fn record_context_compaction_terminal(
    mut flight: CompactionFlight,
    terminal: ContextCompactionTerminalRecord,
    session_store: &Arc<Mutex<SessionStore>>,
    session_id: &SessionId,
    start: &AgentStartPayload,
    canonical_stream: &str,
    event_log: &mut Vec<Envelope>,
    replay_state: &mut AgentReplayState,
    subscribers: &mut Vec<Stream>,
    activity_stats: &mut AgentActivityStatsTracker,
    activity_event_seq: Option<&mut u64>,
) {
    if let Some(task) = flight.fallback_task.take() {
        task.abort();
    }
    if let Some(post_tokens) = terminal.trusted_post_context_tokens {
        let session_id = session_id.clone();
        if let Err(error) = run_session_store_io(session_store, move |store| {
            store.update(&session_id, |record| record.token_count = post_tokens)
        })
        .await
        {
            tracing::warn!(
                operation_id = %flight.operation_id.0,
                %error,
                "failed to persist post-compaction context token count"
            );
        }
    }

    let terminal_state = if terminal.status == ContextCompactionTimelineStatus::Completed {
        StoredCompactionState::Completed
    } else {
        StoredCompactionState::Failed
    };
    let persisted_session_id = session_id.clone();
    let persisted_operation_id = flight.operation_id.clone();
    let persisted_metrics = terminal.metrics.clone();
    let persisted_message = terminal.message.clone();
    let persisted_accepted = terminal.accepted;
    let persisted_mutation = terminal.mutation;
    let resolved_method = resolved_compaction_terminal_method(terminal.method, flight.method);
    let persisted_method = resolved_method;
    if let Err(error) = run_session_store_io(session_store, move |store| {
        store
            .finish_compaction_operation(
                &persisted_session_id,
                FinishCompactionOperation {
                    operation_id: persisted_operation_id,
                    state: terminal_state,
                    accepted: persisted_accepted,
                    mutation: persisted_mutation,
                    method: persisted_method,
                    metrics: persisted_metrics,
                    message: persisted_message,
                },
            )
            .map(|_| ())
    })
    .await
    {
        tracing::error!(
            operation_id = %flight.operation_id.0,
            %error,
            "failed to persist terminal context compaction state"
        );
    }

    let compaction_source_seq = activity_event_seq
        .as_deref()
        .copied()
        .unwrap_or_else(|| activity_stats.stats.source_through_seq.unwrap_or_default());
    let activity_stats_changed = terminal.status == ContextCompactionTimelineStatus::Completed
        && activity_stats.clear_current_context_usage(compaction_source_seq);
    if let Some(activity_event_seq) = activity_event_seq {
        *activity_event_seq = activity_event_seq.saturating_add(1);
    }
    if activity_stats_changed {
        upsert_activity_stats_snapshot(
            canonical_stream,
            event_log,
            subscribers,
            &start.agent_id,
            activity_stats.snapshot(),
        )
        .await;
    }

    let marker_method = resolved_method.unwrap_or(CompactionMethod::NativeRpc);
    let marker = ContextCompactionTimelineEvent {
        marker_id: CompactionObservationId(format!("operation:{}", flight.operation_id.0)),
        operation_id: Some(flight.operation_id.clone()),
        trigger: flight.trigger,
        method: marker_method,
        backend_kind: start.backend_kind,
        provider_session_id: terminal.provider_session_id,
        status: terminal.status,
        mutation: terminal.mutation,
        metrics: terminal.metrics.clone(),
        message: terminal.message.clone(),
        timestamp: now_ms(),
    };
    if let Some(sequence) = append_compaction_marker_once(
        canonical_stream,
        event_log,
        subscribers,
        replay_state,
        &marker,
    )
    .await
    {
        persist_compaction_marker(canonical_stream, session_id, sequence, &marker).await;
    }
    upsert_context_compaction_snapshot(
        canonical_stream,
        event_log,
        subscribers,
        session_id,
        &ContextCompactionNotifyPayload {
            operation_id: flight.operation_id,
            agent_id: start.agent_id.clone(),
            logical_session_id: session_id.clone(),
            backend_kind: start.backend_kind,
            trigger: flight.trigger,
            method: Some(marker_method),
            status: if terminal.status == ContextCompactionTimelineStatus::Completed {
                ContextCompactionStatus::Completed
            } else {
                ContextCompactionStatus::Failed {
                    accepted: terminal.accepted,
                    mutation: terminal.mutation,
                }
            },
            provider_version: flight.provider_version,
            metrics: terminal.metrics,
            message: terminal.message,
        },
    )
    .await;
}

#[allow(clippy::too_many_arguments)]
async fn release_context_compaction_barrier(
    backend: &BackendHandle,
    queue: &mut VecDeque<SequencedQueuedMessage>,
    in_turn: &mut bool,
    idle_transition_armed: &mut bool,
    canonical_stream: &str,
    event_log: &mut Vec<Envelope>,
    subscribers: &mut Vec<Stream>,
    agent_id: &AgentId,
    session_store: &Arc<Mutex<SessionStore>>,
    status_handle: &registry::AgentStatusHandle,
    review_registry: &ReviewRegistryHandle,
    session_id: Option<&SessionId>,
) -> QueuedMessageDispatchOutcome {
    if *in_turn {
        return QueuedMessageDispatchOutcome::Empty;
    }
    dispatch_queued_message(QueuedMessageDispatchContext {
        backend,
        queue,
        in_turn,
        idle_transition_armed,
        canonical_stream,
        event_log,
        subscribers,
        agent_id,
        session_store,
        status_handle,
        review_registry,
        session_id,
    })
    .await
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum QueuedMessageDispatchOutcome {
    Empty,
    Accepted,
    Busy,
    Closed,
}

struct QueuedMessageDispatchContext<'a> {
    backend: &'a BackendHandle,
    queue: &'a mut VecDeque<SequencedQueuedMessage>,
    in_turn: &'a mut bool,
    idle_transition_armed: &'a mut bool,
    canonical_stream: &'a str,
    event_log: &'a mut Vec<Envelope>,
    subscribers: &'a mut Vec<Stream>,
    agent_id: &'a AgentId,
    session_store: &'a Arc<Mutex<SessionStore>>,
    status_handle: &'a registry::AgentStatusHandle,
    review_registry: &'a ReviewRegistryHandle,
    session_id: Option<&'a SessionId>,
}

async fn dispatch_queued_message(
    context: QueuedMessageDispatchContext<'_>,
) -> QueuedMessageDispatchOutcome {
    let Some(queued) = context.queue.pop_front() else {
        return QueuedMessageDispatchOutcome::Empty;
    };
    let review_id = match queued.origin.as_ref() {
        Some(MessageOrigin::Review { review_id }) => Some(review_id.clone()),
        _ => None,
    };
    update_queued_messages_snapshot(
        context.canonical_stream,
        context.event_log,
        context.subscribers,
        context.queue,
        context.session_store,
    )
    .await;
    *context.in_turn = true;
    *context.idle_transition_armed = false;
    match context
        .backend
        .send_with_outcome(AgentInput::SendMessage(queued.clone().into_send_payload()))
        .await
    {
        SendOutcome::Accepted => {
            eprintln!(
                "TYDE RESUME QUEUE DISPATCH agent={} session={} queued_message_id={}",
                context.agent_id,
                context
                    .session_id
                    .map_or("<none>", |session_id| session_id.0.as_str()),
                queued.id,
            );
            mark_agent_turn_active(context.status_handle).await;
            if let Some(review_id) = review_id {
                notify_review_bundle_consumed(context.review_registry, review_id, context.agent_id)
                    .await;
            }
            QueuedMessageDispatchOutcome::Accepted
        }
        SendOutcome::Busy(_) => {
            eprintln!(
                "TYDE RESUME QUEUE BUSY agent={} session={} queued_message_id={}",
                context.agent_id,
                context
                    .session_id
                    .map_or("<none>", |session_id| session_id.0.as_str()),
                queued.id,
            );
            context.queue.push_front(queued);
            update_queued_messages_snapshot(
                context.canonical_stream,
                context.event_log,
                context.subscribers,
                context.queue,
                context.session_store,
            )
            .await;
            mark_agent_turn_active(context.status_handle).await;
            QueuedMessageDispatchOutcome::Busy
        }
        SendOutcome::Closed => {
            *context.in_turn = false;
            eprintln!(
                "TYDE RESUME QUEUE CLOSED agent={} session={} queued_message_id={}",
                context.agent_id,
                context
                    .session_id
                    .map_or("<none>", |session_id| session_id.0.as_str()),
                queued.id,
            );
            QueuedMessageDispatchOutcome::Closed
        }
    }
}

async fn begin_inline_context_fallback(
    context: &mut ContextCompactionDispatchContext<'_>,
    active: &mut CompactionFlight,
    capability: &crate::backend::BackendCompactionCapability,
    message: String,
) -> Result<(), String> {
    if active.fallback_task.is_some()
        || matches!(
            active.state,
            StoredCompactionState::FallbackPreparing | StoredCompactionState::FallbackCommitPending
        )
    {
        return Err("inline fallback is already in progress".to_owned());
    }
    if !context_compaction_fallback_allowed(active.trigger, capability) {
        return Err("supervisor fallback is disabled for an automatic-only backend".to_owned());
    }
    if !transcript_is_authoritative(context.transcript_store, context.session_id).await {
        return Err("inline fallback requires an authoritative transcript".to_owned());
    }

    let transcript_high_water = context.event_log.len() as u64;
    let activity_counter = context.status_handle.snapshot().await.activity_counter;
    let fallback_settings = context.current_session_settings.clone();
    let session_id = context.session_id.clone();
    let operation_id = active.operation_id.clone();
    run_session_store_io(context.session_store, move |store| {
        let mut record = store
            .compaction_operation(&session_id, &operation_id)
            .ok_or_else(|| {
                format!(
                    "missing compaction operation {} before fallback",
                    operation_id.0
                )
            })?;
        record.state = StoredCompactionState::FallbackPreparing;
        record.method = Some(CompactionMethod::InlineFallback);
        record.accepted = false;
        record.mutation = CompactionMutation::NotObserved;
        record.transcript_high_water = transcript_high_water;
        store.put_compaction_operation(&session_id, record)
    })
    .await
    .map_err(|error| format!("failed to persist fallback preparation frontier: {error}"))?;

    mark_inline_context_fallback_preparing(
        active,
        transcript_high_water,
        activity_counter,
        fallback_settings,
    );
    upsert_context_compaction_snapshot(
        context.canonical_stream,
        context.event_log,
        context.subscribers,
        context.session_id,
        &ContextCompactionNotifyPayload {
            operation_id: active.operation_id.clone(),
            agent_id: context.start.agent_id.clone(),
            logical_session_id: context.session_id.clone(),
            backend_kind: context.start.backend_kind,
            trigger: active.trigger,
            method: Some(CompactionMethod::InlineFallback),
            status: ContextCompactionStatus::Started {
                stage: CompactionStage::Finalizing,
            },
            provider_version: active.provider_version.clone(),
            metrics: CompactionMetrics::default(),
            message: Some(message),
        },
    )
    .await;
    let request = PrepareContextFallbackRequest {
        backend_kind: context.start.backend_kind,
        workspace_roots: context.start.workspace_roots.clone(),
        logical_session_id: context.session_id.clone(),
        transcript_store: context.transcript_store.clone(),
        transcript_high_water,
        requested_focus: active.focus.clone(),
        spawn_config: context.spawn_config.clone(),
        use_mock_backend: context.use_mock_backend,
        capacity_tx: context.capacity_tx.clone(),
        antigravity_conversations_dir: context.antigravity_conversations_dir.clone(),
    };
    let operation_id = active.operation_id.clone();
    let tx = context.actor_tx.clone();
    active.fallback_task = Some(tokio::spawn(async move {
        let result = prepare_context_fallback(request).await;
        let _ = tx.send(AgentCommand::ContextCompactionFallbackPrepared {
            operation_id,
            result,
        });
    }));
    Ok(())
}

async fn try_dispatch_context_compaction(
    mut context: ContextCompactionDispatchContext<'_>,
    flight: &mut Option<CompactionFlight>,
    readiness: ContextCompactionDispatchReadiness<'_>,
) {
    let Some(active) = flight.as_mut() else {
        return;
    };
    if !context_compaction_dispatch_is_safe(
        active,
        readiness.queue,
        readiness.in_turn,
        readiness.replay_pending,
        readiness.open_tool_call_ids,
        readiness.pending_tool_response_ids,
        readiness.background_mutation_active,
    ) {
        return;
    }

    let capability = context.backend.compaction_capability();
    if let Some(method) = compaction_method_for_capability(&capability) {
        active.method = Some(method);
    }
    active.state = StoredCompactionState::NativeDispatchPossible;
    let session_id = context.session_id.clone();
    let operation_id = active.operation_id.clone();
    let durable_operation = run_session_store_io(context.session_store, move |store| {
        Ok(store.compaction_operation(&session_id, &operation_id))
    })
    .await;
    let mut record = match durable_operation {
        Ok(Some(record)) => record,
        Ok(None) => {
            let _ = context
                .actor_tx
                .send(AgentCommand::ContextCompactionTerminal {
                    operation_id: active.operation_id.clone(),
                    result: Err(
                        "durable compaction operation disappeared before native dispatch"
                            .to_owned(),
                    ),
                });
            return;
        }
        Err(error) => {
            let _ = context
                .actor_tx
                .send(AgentCommand::ContextCompactionTerminal {
                    operation_id: active.operation_id.clone(),
                    result: Err(format!("failed to read native dispatch frontier: {error}")),
                });
            return;
        }
    };
    match record.state {
        StoredCompactionState::Deferred => {
            record.state = StoredCompactionState::NativeDispatchPossible;
            record.method = active.method;
            let session_id = context.session_id.clone();
            if let Err(error) = run_session_store_io(context.session_store, move |store| {
                store.put_compaction_operation(&session_id, record)
            })
            .await
            {
                let _ = context
                    .actor_tx
                    .send(AgentCommand::ContextCompactionTerminal {
                        operation_id: active.operation_id.clone(),
                        result: Err(format!(
                            "failed to persist native dispatch frontier: {error}"
                        )),
                    });
                return;
            }
        }
        StoredCompactionState::NativeDispatchPossible => {}
        state => {
            let _ = context
                .actor_tx
                .send(AgentCommand::ContextCompactionTerminal {
                    operation_id: active.operation_id.clone(),
                    result: Err(format!(
                        "durable compaction operation entered unexpected state {state:?} before native dispatch"
                    )),
                });
            return;
        }
    }

    upsert_context_compaction_snapshot(
        context.canonical_stream,
        context.event_log,
        context.subscribers,
        context.session_id,
        &ContextCompactionNotifyPayload {
            operation_id: active.operation_id.clone(),
            agent_id: context.start.agent_id.clone(),
            logical_session_id: context.session_id.clone(),
            backend_kind: context.start.backend_kind,
            trigger: active.trigger,
            method: None,
            status: ContextCompactionStatus::Started {
                stage: CompactionStage::Dispatching,
            },
            provider_version: active.provider_version.clone(),
            metrics: CompactionMetrics::default(),
            message: None,
        },
    )
    .await;

    let request = crate::backend::BackendCompactionRequest {
        operation_id: active.operation_id.clone(),
        trigger: active.trigger,
        focus: active.focus.clone(),
        transcript_authoritative: transcript_is_authoritative(
            context.transcript_store,
            context.session_id,
        )
        .await,
    };
    match context.backend.begin_compaction(request).await {
        crate::backend::BackendCompactionStart::Accepted(accepted) => {
            active.state = StoredCompactionState::NativeAccepted;
            active.terminal_taken = true;
            let session_id = context.session_id.clone();
            let operation_id = active.operation_id.clone();
            let accepted_persisted = run_session_store_io(context.session_store, move |store| {
                let mut record = store
                    .compaction_operation(&session_id, &operation_id)
                    .ok_or_else(|| {
                        format!(
                            "missing compaction operation {} after native acceptance",
                            operation_id.0
                        )
                    })?;
                record.state = StoredCompactionState::NativeAccepted;
                record.accepted = true;
                store.put_compaction_operation(&session_id, record)
            })
            .await;
            if let Err(error) = accepted_persisted {
                let _ = context
                    .actor_tx
                    .send(AgentCommand::ContextCompactionTerminal {
                        operation_id: active.operation_id.clone(),
                        result: Err(format!(
                            "failed to persist native acceptance frontier: {error}"
                        )),
                    });
                return;
            }
            let operation_id = accepted.operation_id;
            let tx = context.actor_tx.clone();
            tokio::spawn(async move {
                let result = accepted.terminal.await.map_err(|_| {
                    "accepted backend compaction ended without a terminal result".to_owned()
                });
                let _ = tx.send(AgentCommand::ContextCompactionTerminal {
                    operation_id,
                    result,
                });
            });
        }
        crate::backend::BackendCompactionStart::Deferred { reason } => {
            active.state = StoredCompactionState::Deferred;
            upsert_context_compaction_snapshot(
                context.canonical_stream,
                context.event_log,
                context.subscribers,
                context.session_id,
                &ContextCompactionNotifyPayload {
                    operation_id: active.operation_id.clone(),
                    agent_id: context.start.agent_id.clone(),
                    logical_session_id: context.session_id.clone(),
                    backend_kind: context.start.backend_kind,
                    trigger: active.trigger,
                    method: None,
                    status: ContextCompactionStatus::Deferred {
                        stage: CompactionStage::WaitingForIdle,
                    },
                    provider_version: active.provider_version.clone(),
                    metrics: CompactionMetrics::default(),
                    message: Some(format!("backend deferred compaction: {reason:?}")),
                },
            )
            .await;
            arm_context_compaction_retry(active, context.actor_tx);
        }
        crate::backend::BackendCompactionStart::NotDispatched {
            reason,
            fallback_safe,
        } => {
            if !fallback_safe {
                let _ = context
                    .actor_tx
                    .send(AgentCommand::ContextCompactionTerminal {
                        operation_id: active.operation_id.clone(),
                        result: Err(format!(
                            "native compaction was not safely dispatched: {reason:?}"
                        )),
                    });
                return;
            }
            if let Err(error) = begin_inline_context_fallback(
                &mut context,
                active,
                &capability,
                format!(
                    "native compaction was not dispatched ({reason:?}); preparing inline fallback"
                ),
            )
            .await
            {
                let _ = context
                    .actor_tx
                    .send(AgentCommand::ContextCompactionTerminal {
                        operation_id: active.operation_id.clone(),
                        result: Err(error),
                    });
            }
        }
        crate::backend::BackendCompactionStart::DispatchUncertain(result) => {
            active.state = StoredCompactionState::NativeAccepted;
            active.terminal_taken = true;
            let _ = context
                .actor_tx
                .send(AgentCommand::ContextCompactionTerminal {
                    operation_id: active.operation_id.clone(),
                    result: Ok(*result),
                });
        }
    }
}

async fn upsert_context_compaction_snapshot(
    canonical_stream: &str,
    event_log: &mut Vec<Envelope>,
    subscribers: &mut Vec<Stream>,
    authoritative_logical_session_id: &SessionId,
    payload: &ContextCompactionNotifyPayload,
) {
    let mut payload = payload.clone();
    payload
        .logical_session_id
        .clone_from(authoritative_logical_session_id);
    let value =
        serde_json::to_value(&payload).expect("failed to serialize context compaction payload");
    if let Some(snapshot) = event_log
        .iter_mut()
        .find(|event| event.kind == FrameKind::ContextCompactionNotify)
    {
        snapshot.payload = value;
    } else {
        event_log.push(Envelope {
            stream: protocol::StreamPath(canonical_stream.to_owned()),
            kind: FrameKind::ContextCompactionNotify,
            seq: event_log.len() as u64,
            payload: value,
        });
    }
    broadcast_live_event(subscribers, FrameKind::ContextCompactionNotify, &payload).await;
}

async fn upsert_context_compaction_capability(
    canonical_stream: &str,
    event_log: &mut Vec<Envelope>,
    subscribers: &mut Vec<Stream>,
    payload: &ContextCompactionCapabilityPayload,
) {
    let value =
        serde_json::to_value(payload).expect("failed to serialize compaction capability payload");
    if let Some(snapshot) = event_log
        .iter_mut()
        .find(|event| event.kind == FrameKind::ContextCompactionCapability)
    {
        snapshot.payload = value;
    } else {
        event_log.push(Envelope {
            stream: protocol::StreamPath(canonical_stream.to_owned()),
            kind: FrameKind::ContextCompactionCapability,
            seq: event_log.len() as u64,
            payload: value,
        });
    }
    broadcast_live_event(subscribers, FrameKind::ContextCompactionCapability, payload).await;
}

async fn append_compaction_marker_once(
    canonical_stream: &str,
    event_log: &mut Vec<Envelope>,
    subscribers: &mut Vec<Stream>,
    replay_state: &mut AgentReplayState,
    marker: &ContextCompactionTimelineEvent,
) -> Option<u64> {
    let existing_sequence = event_log.iter().find_map(|envelope| {
        (envelope.kind == FrameKind::ChatEvent
            && envelope
                .parse_payload::<ChatEvent>()
                .ok()
                .is_some_and(|event| {
                    matches!(
                        event,
                        ChatEvent::ContextCompaction(existing)
                            if existing.marker_id == marker.marker_id
                    )
                }))
        .then_some(envelope.seq)
    });
    if existing_sequence.is_some() {
        return existing_sequence;
    }
    let insertion_index = event_log.len();
    append_chat_event(
        canonical_stream,
        event_log,
        subscribers,
        replay_state,
        &ChatEvent::ContextCompaction(marker.clone()),
    )
    .await;
    event_log.get(insertion_index).map(|envelope| envelope.seq)
}

async fn persist_compaction_marker(
    canonical_stream: &str,
    session_id: &SessionId,
    sequence: u64,
    marker: &ContextCompactionTimelineEvent,
) {
    let Some(store) = transcript_session_registry()
        .lock()
        .expect("transcript session registry poisoned")
        .get(canonical_stream)
        .map(|registered| registered.store.clone())
    else {
        return;
    };
    if !store.actor_io_enabled() {
        return;
    }
    let record = crate::store::transcript::TranscriptRecord {
        logical_session_id: session_id.clone(),
        sequence,
        event_id: marker.marker_id.0.clone(),
        visibility: crate::store::transcript::TranscriptVisibility::TimelineMarker,
        provider_identity: marker
            .provider_session_id
            .as_ref()
            .map(
                |provider_session_id| crate::store::transcript::ProviderEventIdentity {
                    backend: format!("{:?}", marker.backend_kind).to_ascii_lowercase(),
                    provider_session_id: provider_session_id.0.clone(),
                    event_id: marker.marker_id.0.clone(),
                },
            ),
        event: ChatEvent::ContextCompaction(marker.clone()),
        timestamp_ms: marker.timestamp,
    };
    let session_id = session_id.clone();
    let marker_id = marker.marker_id.0.clone();
    let persisted =
        tokio::task::spawn_blocking(move || store.append_import_if_missing(&record).map(|_| ()))
            .await;
    match persisted {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            tracing::warn!(
                session_id = %session_id,
                marker_id = %marker_id,
                %error,
                "failed to persist compaction timeline marker"
            );
        }
        Err(error) => {
            tracing::warn!(
                session_id = %session_id,
                marker_id = %marker_id,
                %error,
                "compaction marker persistence task failed"
            );
        }
    }
}

async fn broadcast_live_event<T: serde::Serialize>(
    subscribers: &mut Vec<Stream>,
    kind: FrameKind,
    payload: &T,
) {
    let payload = serde_json::to_value(payload)
        .expect("failed to serialize live protocol payload in agent actor");
    let event = Envelope {
        stream: protocol::StreamPath(String::new()),
        kind,
        seq: 0,
        payload,
    };
    broadcast_event(subscribers, &event);
}

fn broadcast_event(subscribers: &mut Vec<Stream>, event: &Envelope) {
    let mut idx = 0;
    while idx < subscribers.len() {
        if subscribers[idx]
            .send_value(event.kind, event.payload.clone())
            .is_err()
        {
            subscribers.swap_remove(idx);
            continue;
        }
        idx += 1;
    }
}

fn attach_subscriber_with_latest_output(
    event_log: &[Envelope],
    replay_state: Option<&AgentReplayState>,
    latest_output: &AgentControlOutput,
    turn_active: bool,
    subscribers: &mut Vec<Stream>,
    stream: Stream,
) -> bool {
    let stream_path = stream.path().clone();
    let mut events = agent_bootstrap_events_from_log(event_log);
    let history_entries = filtered_session_history_entries_from_log(event_log, replay_state);
    let history_tail = initial_history_tail_entries(&history_entries);
    if let Some((oldest_tail_seq, _)) = history_tail.first() {
        let prior_history_count = prior_history_message_count(&history_entries, *oldest_tail_seq);
        if prior_history_count > 0 {
            events.push(AgentBootstrapEvent::HasPriorHistory {
                message_count: prior_history_count,
                before_seq: *oldest_tail_seq,
            });
        }
    }
    events.extend(
        history_tail
            .into_iter()
            .map(|(_, event)| AgentBootstrapEvent::ChatEvent(event)),
    );
    if let Some(replay_state) = replay_state {
        events.extend(
            replay_state
                .active_stream_events()
                .into_iter()
                .map(AgentBootstrapEvent::ChatEvent),
        );
        let mut active_tool_progress = replay_state
            .active_tool_progress
            .values()
            .cloned()
            .collect::<Vec<_>>();
        active_tool_progress.sort_by(|left, right| left.tool_call_id.cmp(&right.tool_call_id));
        for progress in active_tool_progress {
            let already_included = events.iter().any(|event| {
                matches!(
                    event,
                    AgentBootstrapEvent::ChatEvent(ChatEvent::ToolProgress(included))
                        if included.tool_call_id == progress.tool_call_id
                )
            });
            if !already_included {
                events.push(AgentBootstrapEvent::ChatEvent(ChatEvent::ToolProgress(
                    progress,
                )));
            }
        }
        if replay_state.resume_history_settled_idle {
            events.push(AgentBootstrapEvent::ChatEvent(
                ChatEvent::TypingStatusChanged(false),
            ));
        }
    }

    let bootstrap_event_count = events.len();
    let payload = serde_json::to_value(AgentBootstrapPayload {
        events,
        latest_output: latest_output.clone(),
        turn_active,
    })
    .expect("failed to serialize AgentBootstrap payload");
    if stream
        .send_value(FrameKind::AgentBootstrap, payload)
        .is_err()
    {
        return false;
    }

    subscribers.push(stream);
    tracing::debug!(
        stream = %stream_path,
        bootstrap_event_count,
        "activated agent subscriber after AgentBootstrap"
    );
    true
}

fn filtered_session_history_entries_from_log(
    event_log: &[Envelope],
    _replay_state: Option<&AgentReplayState>,
) -> Vec<(u64, ChatEvent)> {
    session_history_entries_from_log(event_log)
}

fn initial_history_tail_entries(entries: &[(u64, ChatEvent)]) -> Vec<(u64, ChatEvent)> {
    let start = history_start_for_message_limit(entries, entries.len(), INITIAL_HISTORY_TAIL_LIMIT);
    entries[start..].to_vec()
}

fn agent_bootstrap_events_from_log(event_log: &[Envelope]) -> Vec<AgentBootstrapEvent> {
    let mut events = Vec::new();
    for envelope in event_log {
        if envelope.kind == FrameKind::ContextCompactionNotify
            && envelope
                .parse_payload::<ContextCompactionNotifyPayload>()
                .ok()
                .is_none_or(|payload| payload.status.is_terminal())
        {
            continue;
        }
        if matches!(
            envelope.kind,
            FrameKind::AgentStart
                | FrameKind::AgentError
                | FrameKind::SessionSettings
                | FrameKind::QueuedMessages
                | FrameKind::AgentActivityStats
                | FrameKind::ContextCompactionNotify
                | FrameKind::ContextCompactionCapability
        ) {
            events.push(agent_bootstrap_event_from_envelope(envelope));
        }
    }
    events
}

fn prior_history_message_count(entries: &[(u64, ChatEvent)], before_seq: u64) -> u32 {
    entries
        .iter()
        .filter(|(seq, event)| *seq < before_seq && history_message_terminal(event))
        .count()
        .min(u32::MAX as usize) as u32
}

fn session_history_window(
    event_log: &[Envelope],
    before_seq: Option<u64>,
    limit: usize,
    replay_state: Option<&AgentReplayState>,
) -> SessionHistoryWindow {
    let entries = filtered_session_history_entries_from_log(event_log, replay_state);
    let eligible_end = entries
        .iter()
        .position(|(seq, _)| before_seq.is_some_and(|before_seq| *seq >= before_seq))
        .unwrap_or(entries.len());
    let limit = limit.max(1);
    let start = history_start_for_message_limit(&entries, eligible_end, limit);
    let selected = &entries[start..eligible_end];
    SessionHistoryWindow {
        events: selected
            .iter()
            .rev()
            .map(|(_, event)| event.clone())
            .collect(),
        has_more_before: start > 0,
        oldest_seq: selected.first().map(|(seq, _)| *seq),
    }
}

async fn authoritative_session_history_window(
    store: &TranscriptStore,
    session_id: &SessionId,
    before_seq: Option<u64>,
    limit: usize,
    _completed_stream_filter: Option<()>,
) -> Option<SessionHistoryWindow> {
    if !store.actor_io_enabled() {
        return None;
    }
    let session_id = session_id.clone();
    let store = store.clone();
    tokio::task::spawn_blocking(move || {
        if !store.is_authoritative(&session_id) {
            return None;
        }
        let entries = store
            .load(&session_id)
            .ok()?
            .into_iter()
            .filter(|record| {
                before_seq.is_none_or(|before| record.sequence < before)
                    && matches!(
                        record.visibility,
                        crate::store::transcript::TranscriptVisibility::Visible
                            | crate::store::transcript::TranscriptVisibility::TimelineMarker
                    )
            })
            .map(|record| (record.sequence, record.event))
            .collect::<Vec<_>>();
        let entries = project_session_history_entries(entries);
        let end = entries.len();
        let start = history_start_for_message_limit(&entries, end, limit.max(1));
        let selected = &entries[start..end];
        let oldest_seq = selected.first().map(|(sequence, _)| *sequence);
        Some(SessionHistoryWindow {
            events: selected
                .iter()
                .rev()
                .map(|(_, event)| event.clone())
                .collect(),
            has_more_before: start > 0,
            oldest_seq,
        })
    })
    .await
    .ok()
    .flatten()
}

fn history_start_for_message_limit(
    entries: &[(u64, ChatEvent)],
    end: usize,
    limit: usize,
) -> usize {
    let message_count = entries[..end]
        .iter()
        .filter(|(_, event)| history_message_terminal(event))
        .count();
    if message_count <= limit {
        return 0;
    }

    let messages_to_skip = message_count - limit;
    let mut skipped = 0;
    entries[..end]
        .iter()
        .position(|(_, event)| {
            if !history_message_terminal(event) {
                return false;
            }
            if skipped == messages_to_skip {
                return true;
            }
            skipped += 1;
            false
        })
        .map(|terminal_index| history_message_start(entries, terminal_index))
        .expect("message_count > limit requires a history window start message")
}

fn history_message_terminal(event: &ChatEvent) -> bool {
    matches!(event, ChatEvent::MessageAdded(_) | ChatEvent::StreamEnd(_))
}

fn history_message_start(entries: &[(u64, ChatEvent)], terminal_index: usize) -> usize {
    let ChatEvent::StreamEnd(_) = &entries[terminal_index].1 else {
        return terminal_index;
    };
    for index in (0..terminal_index).rev() {
        match &entries[index].1 {
            ChatEvent::StreamStart(_) => return index,
            ChatEvent::MessageAdded(_) | ChatEvent::StreamEnd(_) => break,
            _ => {}
        }
    }
    terminal_index
}

/// Older session logs persisted provider-native collaboration payloads as
/// unrestricted `Other` values. Project only shapes that carry an explicit
/// Claude/Codex collaboration fingerprint; unrelated `Other` tools remain
/// byte-for-byte unchanged.
fn project_legacy_native_collaboration_event(event: &mut ChatEvent) {
    match event {
        ChatEvent::ToolRequest(request) => {
            let ToolRequestType::Other { args } = &request.tool_type else {
                return;
            };
            if legacy_codex_collaboration_value(args) {
                let prompt = nonempty_json_string(args, "prompt");
                let name = nonempty_json_string(args, "receiverAgentName");
                let action = args
                    .get("tool")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("collaboration");
                if prompt.is_some()
                    && matches!(action.to_ascii_lowercase().as_str(), "spawn" | "spawnagent")
                {
                    request.tool_type = ToolRequestType::AgentSpawn {
                        prompt,
                        name,
                        execution_mode: protocol::AgentExecutionMode::Background,
                    };
                } else {
                    request.tool_type = ToolRequestType::Other {
                        args: serde_json::json!({
                            "action": action,
                            "agent_count": legacy_codex_agent_count(args),
                        }),
                    };
                }
            } else if legacy_claude_agent_request(args) {
                let prompt = ["prompt", "task", "instruction", "message"]
                    .into_iter()
                    .find_map(|key| nonempty_json_string(args, key));
                let name = nonempty_json_string(args, "description")
                    .or_else(|| nonempty_json_string(args, "subagent_type"));
                let execution_mode = if args
                    .get("run_in_background")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false)
                {
                    protocol::AgentExecutionMode::Background
                } else {
                    protocol::AgentExecutionMode::Foreground
                };
                request.tool_type = ToolRequestType::AgentSpawn {
                    prompt,
                    name,
                    execution_mode,
                };
            }
        }
        ChatEvent::ToolExecutionCompleted(completion) => {
            let ToolExecutionOutcome::Succeeded {
                result: ToolExecutionResult::Other { result },
            } = &mut completion.outcome
            else {
                return;
            };
            if legacy_codex_collaboration_value(result) {
                let action = result
                    .get("tool")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("collaboration")
                    .to_owned();
                let agent_count = legacy_codex_agent_count(result);
                *result = serde_json::json!({
                    "action": action,
                    "status": "completed",
                    "agent_count": agent_count,
                });
            } else if legacy_claude_agent_result(result) {
                *result = serde_json::json!({
                    "status": "completed",
                });
            }
        }
        _ => {}
    }
}

fn nonempty_json_string(value: &serde_json::Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn legacy_codex_collaboration_value(value: &serde_json::Value) -> bool {
    matches!(
        value.get("type").and_then(serde_json::Value::as_str),
        Some("collabToolCall" | "collabAgentToolCall")
    )
}

fn legacy_codex_agent_count(value: &serde_json::Value) -> usize {
    if let Some(ids) = value
        .get("receiverThreadIds")
        .and_then(serde_json::Value::as_array)
    {
        return ids.len();
    }
    if value.get("receiverThreadId").is_some() {
        return 1;
    }
    value
        .get("agentsStates")
        .and_then(serde_json::Value::as_object)
        .map_or(0, serde_json::Map::len)
}

fn legacy_claude_agent_request(args: &serde_json::Value) -> bool {
    args.get("prompt").is_some()
        && [
            "subagent_type",
            "run_in_background",
            "description",
            "resume",
        ]
        .into_iter()
        .any(|key| args.get(key).is_some())
}

fn legacy_claude_agent_result(result: &serde_json::Value) -> bool {
    [
        "agentId",
        "agent_id",
        "session_id",
        "task_id",
        "output_file",
    ]
    .into_iter()
    .any(|key| result.get(key).is_some())
}

fn session_history_entries_from_log(event_log: &[Envelope]) -> Vec<(u64, ChatEvent)> {
    let entries = event_log
        .iter()
        .filter(|envelope| envelope.kind == FrameKind::ChatEvent)
        .map(|envelope| {
            (
                envelope.seq,
                serde_json::from_value(envelope.payload.clone())
                    .expect("failed to parse ChatEvent from replay log"),
            )
        });
    project_session_history_entries(entries)
}

fn project_session_history_entries(
    entries: impl IntoIterator<Item = (u64, ChatEvent)>,
) -> Vec<(u64, ChatEvent)> {
    let mut events = Vec::new();
    for (sequence, mut event) in entries {
        project_legacy_native_collaboration_event(&mut event);
        match event {
            ChatEvent::MessageMetadataUpdated(update) => {
                if !fold_message_metadata_update_into_history_events(&mut events, &update) {
                    tracing::warn!(
                        message_id = %update.message_id,
                        "skipping MessageMetadataUpdated without a matching history message"
                    );
                }
            }
            ChatEvent::TypingStatusChanged(_) => {}
            event => events.push((sequence, event)),
        }
    }
    events
}

fn fold_message_metadata_update_into_history_events(
    events: &mut [(u64, ChatEvent)],
    update: &MessageMetadataUpdateData,
) -> bool {
    for event in events.iter_mut().rev() {
        let message = match &mut event.1 {
            ChatEvent::MessageAdded(message) => message,
            ChatEvent::StreamEnd(end) => &mut end.message,
            _ => continue,
        };
        if message.message_id.as_ref() != Some(&update.message_id) {
            continue;
        }
        if update.model_info.is_some() {
            message.model_info = update.model_info.clone();
        }
        if update.token_usage.is_some() {
            message.token_usage = update.token_usage.clone();
        }
        if update.context_breakdown.is_some() {
            message.context_breakdown = update.context_breakdown.clone();
        }
        return true;
    }
    false
}

fn agent_bootstrap_event_from_envelope(envelope: &Envelope) -> AgentBootstrapEvent {
    match envelope.kind {
        FrameKind::AgentStart => AgentBootstrapEvent::AgentStart(
            serde_json::from_value(envelope.payload.clone())
                .expect("failed to parse AgentStart from replay log"),
        ),
        FrameKind::AgentError => AgentBootstrapEvent::AgentError(
            serde_json::from_value(envelope.payload.clone())
                .expect("failed to parse AgentError from replay log"),
        ),
        FrameKind::SessionSettings => AgentBootstrapEvent::SessionSettings(
            serde_json::from_value(envelope.payload.clone())
                .expect("failed to parse SessionSettings from replay log"),
        ),
        FrameKind::QueuedMessages => AgentBootstrapEvent::QueuedMessages(
            serde_json::from_value(envelope.payload.clone())
                .expect("failed to parse QueuedMessages from replay log"),
        ),
        FrameKind::AgentActivityStats => AgentBootstrapEvent::AgentActivityStats(
            serde_json::from_value(envelope.payload.clone())
                .expect("failed to parse AgentActivityStats from replay log"),
        ),
        FrameKind::ContextCompactionNotify => AgentBootstrapEvent::ContextCompaction(
            serde_json::from_value(envelope.payload.clone())
                .expect("failed to parse ContextCompactionNotify from replay log"),
        ),
        FrameKind::ContextCompactionCapability => AgentBootstrapEvent::ContextCompactionCapability(
            serde_json::from_value(envelope.payload.clone())
                .expect("failed to parse ContextCompactionCapability from replay log"),
        ),
        FrameKind::ChatEvent => AgentBootstrapEvent::ChatEvent(
            serde_json::from_value(envelope.payload.clone())
                .expect("failed to parse ChatEvent from replay log"),
        ),
        other => panic!("unsupported agent replay event kind {other} in AgentBootstrap"),
    }
}

async fn apply_runtime_session_updates(
    session_store: &Arc<Mutex<SessionStore>>,
    session_id: &SessionId,
    event: &ChatEvent,
) -> Option<SessionSummaryCountUpdatedPayload> {
    let mut count_update = None;
    let result = {
        let store = session_store.lock().await;
        match event {
            ChatEvent::StreamEnd(data) => store.update(session_id, |record| {
                record.updated_at_ms = now_ms();
                record.message_count += 1;
                count_update = Some(SessionSummaryCountUpdatedPayload {
                    session_id: session_id.clone(),
                    assistant_turn_count: record.message_count,
                    updated_at_ms: record.updated_at_ms,
                });
                if let Some(delta) =
                    known_turn_usage(&data.message.token_usage).map(|usage| usage.total_tokens)
                {
                    record.token_count =
                        Some(record.token_count.unwrap_or(0).saturating_add(delta));
                }
            }),
            ChatEvent::MessageMetadataUpdated(data) => store.update(session_id, |record| {
                record.updated_at_ms = now_ms();
                if let Some(delta) =
                    known_turn_usage(&data.token_usage).map(|usage| usage.total_tokens)
                {
                    record.token_count =
                        Some(record.token_count.unwrap_or(0).saturating_add(delta));
                }
            }),
            ChatEvent::TaskUpdate(tasks) => {
                let title = tasks.title.trim();
                tracing::info!(
                    session_id = %session_id,
                    task_count = tasks.tasks.len(),
                    "persisting typed task state"
                );
                store
                    .set_task_list(session_id, tasks.clone())
                    .and_then(|()| {
                        store.update(session_id, |record| {
                            record.updated_at_ms = now_ms();
                            if !title.is_empty() && record.alias.is_none() {
                                record.alias = Some(title.to_string());
                            }
                        })
                    })
            }
            _ => store.update(session_id, |record| {
                record.updated_at_ms = now_ms();
            }),
        }
    };

    if let Err(err) = result {
        tracing::error!("failed to update session store for {}: {}", session_id, err);
        return None;
    }
    count_update
}

pub(crate) fn build_name_generation_prompt(prompt: &str) -> String {
    format!(
        "Return only a short 2-4 word work name for this request. No quotes, no markdown, no explanation. Request: {prompt}"
    )
}

fn build_activity_summary_prompt(rendered_history: &str, previous_summary: Option<&str>) -> String {
    let previous = previous_summary
        .filter(|summary| !summary.trim().is_empty())
        .unwrap_or("None");
    format!(
        "You summarize live coding-agent activity for a UI.\n\
Return one concise sentence, max 18 words.\n\
Describe what the agent is currently doing or just finished.\n\
Do not mention that you are summarizing. Do not invent facts.\n\
If the input is insufficient, return exactly: No clear activity yet.\n\n\
Previous summary: {previous}\n\
Recent activity:\n{rendered_history}"
    )
}

async fn generate_mock_activity_summary(
    request: GenerateAgentActivitySummaryRequest,
) -> Result<AgentActivitySummary, String> {
    if request
        .rendered_history
        .contains("__mock_slow_activity_summary__")
    {
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    if request
        .rendered_history
        .contains("__mock_fail_activity_summary__")
    {
        return Err("mock activity summary failure".to_owned());
    }
    Ok(AgentActivitySummary {
        text: "Mock summary: agent is working on recent activity".to_owned(),
        generated_at_ms: now_ms(),
        source_from_seq: request.source_from_seq,
        source_through_seq: request.source_through_seq,
    })
}

fn sanitize_activity_summary_text(text: &str) -> Result<String, String> {
    let stripped = strip_wrapping_quotes(text.trim());
    let collapsed = stripped.split_whitespace().collect::<Vec<_>>().join(" ");
    let without_markdown = collapsed
        .trim_matches(|ch: char| matches!(ch, '*' | '_' | '`' | '#' | '-' | '•'))
        .trim()
        .to_owned();
    if without_markdown.is_empty() {
        return Err("activity summary was empty".to_owned());
    }
    Ok(without_markdown.chars().take(180).collect())
}

pub(crate) fn derive_agent_name(prompt: &str) -> String {
    let trimmed = prompt.trim();
    if trimmed.is_empty() {
        return IMAGE_ONLY_AGENT_NAME.to_string();
    }

    generate_mock_name(trimmed).unwrap_or_else(|fallback_err| {
        tracing::error!(
            "prompt-derived agent name fallback failed for prompt {:?}: {}",
            trimmed,
            fallback_err
        );
        IMAGE_ONLY_AGENT_NAME.to_string()
    })
}

fn generate_mock_name(prompt: &str) -> Result<String, String> {
    if prompt.contains("__mock_fail_agent_name__") {
        return Err("mock agent name generation failure".to_owned());
    }
    let mut words = extract_name_words(prompt);
    if words.is_empty() {
        words = vec!["New".to_string(), "Agent".to_string(), "Task".to_string()];
    }
    words.truncate(4);
    while words.len() < 2 {
        words.push("Task".to_string());
    }
    sanitize_generated_agent_name(&words.join(" "))
}

fn sanitize_generated_agent_name(name: &str) -> Result<String, String> {
    let stripped = strip_wrapping_quotes(name.trim());
    if stripped.is_empty() {
        return Err("generated agent name was empty".to_string());
    }

    let mut words = stripped
        .split_whitespace()
        .map(clean_name_word)
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>();

    // Accept whatever usable text the model produced. The prompt asks for 2-4
    // words, but a short answer ("Greeting") is still a better name than
    // discarding the generation; an overlong one is truncated rather than
    // rejected.
    if words.is_empty() {
        return Err(format!(
            "generated agent name contained no usable words, got {:?}",
            stripped
        ));
    }
    words.truncate(4);

    for word in &mut words {
        *word = title_case_word(word);
    }

    Ok(words.join(" "))
}

fn strip_wrapping_quotes(mut value: &str) -> &str {
    loop {
        let trimmed = value.trim();
        let bytes = trimmed.as_bytes();
        if bytes.len() < 2 {
            return trimmed;
        }
        let first = bytes[0] as char;
        let last = bytes[bytes.len() - 1] as char;
        let wrapped = matches!((first, last), ('\"', '\"') | ('\'', '\'') | ('`', '`'));
        if !wrapped {
            return trimmed;
        }
        value = &trimmed[1..trimmed.len() - 1];
    }
}

fn extract_name_words(prompt: &str) -> Vec<String> {
    const STOPWORDS: &[&str] = &[
        "a", "an", "and", "at", "based", "by", "for", "from", "how", "i", "if", "in", "into",
        "make", "new", "of", "on", "or", "please", "so", "that", "the", "this", "to", "update",
        "with", "you",
    ];

    let mut words = Vec::new();
    for raw in prompt.split_whitespace() {
        let cleaned = clean_name_word(raw);
        if cleaned.is_empty() {
            continue;
        }
        if STOPWORDS.contains(&cleaned.to_ascii_lowercase().as_str()) {
            continue;
        }
        words.push(title_case_word(&cleaned));
    }
    words
}

fn clean_name_word(word: &str) -> String {
    word.trim_matches(|ch: char| !ch.is_ascii_alphanumeric() && ch != '-' && ch != '_')
        .replace('_', " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join("-")
}

fn title_case_word(word: &str) -> String {
    let mut chars = word.chars();
    let Some(first) = chars.next() else {
        return String::new();
    };
    let mut out = String::new();
    out.extend(first.to_uppercase());
    out.push_str(&chars.as_str().to_ascii_lowercase());
    out
}
