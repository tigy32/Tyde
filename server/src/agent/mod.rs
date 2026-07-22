use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use protocol::types::StreamIdentityViolation;
use protocol::{
    AgentActivityStats, AgentActivityStatsPayload, AgentActivitySummary, AgentBootstrapEvent,
    AgentBootstrapPayload, AgentControlLatestOutput, AgentControlOutput, AgentErrorCode,
    AgentErrorPayload, AgentId, AgentInput, AgentOrigin, AgentRenamedPayload, AgentStartPayload,
    BackendAccessMode, BackendKind, ChatEvent, ChatMessage, ChatMessageId, Envelope, FrameKind,
    MessageMetadataUpdateData, MessageOrigin, MessageSender, MessageTokenUsage, ModelInfo,
    ModelRequestId, ModelRequestTokenUsage, QueuedMessageEntry, QueuedMessageId,
    QueuedMessagesPayload, ReasoningData, ReviewErrorContext, SendMessagePayload, SessionId,
    SessionSettingsPayload, SessionSettingsValues, SpawnCostHint, StreamEndData, StreamStartData,
    StreamTextDeltaData, TaskTokenUsageAmount, TaskTokenUsageScope,
    TaskTokenUsageUnavailableReason, TokenUsage, TokenUsageScope, TokenUsageUnavailableReason,
    ToolExecutionCompletedData, ToolExecutionResult, ToolPolicy, ToolRequestType,
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
    validate_session_settings_values,
};
use crate::host::{HostCapacityTx, HostSubAgentEmitter};
use crate::review::ReviewRegistryHandle;
use crate::store::session::SessionStore;
use crate::stream::Stream;
use crate::sub_agent::HostSubAgentSpawnTx;

pub(crate) mod customization;
pub(crate) mod registry;
pub(crate) mod supervisor;

use self::registry::{
    AgentStartupFailure, InitialAgentAlias, InitialAgentAliasPersistence, ResolvedSpawnRequest,
};

const IMAGE_ONLY_AGENT_NAME: &str = "Image Review Task";
const BACKEND_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);
const RESUME_REPLAY_BARRIER_TIMEOUT: Duration = Duration::from_secs(30);
const INITIAL_HISTORY_TAIL_LIMIT: usize = 15;
pub(crate) const DEFAULT_COMPACTION_SUMMARY_MAX_BYTES: usize = 32 * 1024;
pub(crate) const MAX_COMPACTION_SUMMARY_BYTES: usize = 128 * 1024;

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
    queue: &'a mut VecDeque<QueuedMessageEntry>,
}

struct InitialFollowUpContext<'a> {
    backend: &'a mut Option<BackendHandle>,
    in_turn: &'a mut bool,
    idle_transition_armed: &'a mut bool,
    session_store: &'a Arc<Mutex<SessionStore>>,
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
    queue: &'a mut VecDeque<QueuedMessageEntry>,
    pending_inputs: &'a mut VecDeque<AgentInput>,
    rx: &'a mut mpsc::UnboundedReceiver<AgentCommand>,
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
    pub(crate) host_sub_agent_spawn_tx: HostSubAgentSpawnTx,
    pub(crate) capacity_tx: HostCapacityTx,
    pub(crate) review_registry: ReviewRegistryHandle,
    pub(crate) status_handle: registry::AgentStatusHandle,
    pub(crate) antigravity_conversations_dir: PathBuf,
}

pub(crate) struct AgentActorRuntimeResources {
    pub(crate) session_store: Arc<Mutex<SessionStore>>,
    pub(crate) host_sub_agent_spawn_tx: HostSubAgentSpawnTx,
    pub(crate) capacity_tx: HostCapacityTx,
    pub(crate) review_registry: ReviewRegistryHandle,
    pub(crate) antigravity_conversations_dir: PathBuf,
}

impl AgentActorRuntimeResources {
    pub(crate) fn with_status(
        self,
        status_handle: registry::AgentStatusHandle,
    ) -> AgentActorRuntimeContext {
        AgentActorRuntimeContext {
            session_store: self.session_store,
            host_sub_agent_spawn_tx: self.host_sub_agent_spawn_tx,
            capacity_tx: self.capacity_tx,
            review_registry: self.review_registry,
            status_handle,
            antigravity_conversations_dir: self.antigravity_conversations_dir,
        }
    }
}

enum AgentCommand {
    SendInput(AgentInput),
    Compact {
        summary_prompt: String,
        max_summary_bytes: usize,
        reply: oneshot::Sender<Result<CompactionSummary, String>>,
    },
    CompactIfInactive {
        expected_activity_counter: u64,
        summary_prompt: String,
        max_summary_bytes: usize,
        accepted: oneshot::Sender<Result<(), String>>,
        reply: oneshot::Sender<Result<CompactionSummary, String>>,
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
    ReadSupervisionContext {
        reply: oneshot::Sender<supervisor::SupervisionContextSnapshot>,
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

#[derive(Default)]
struct AgentReplayState {
    active_stream: Option<ReplayActiveStream>,
    completed_stream: Option<ReplayCompletedStream>,
    terminal_stream_message_ids: HashSet<ChatMessageId>,
    recorded_message_senders: HashMap<ChatMessageId, MessageSender>,
    typing: bool,
    resume_history_settled_idle: bool,
    /// Position in the event_log of the single retained `ToolProgress`
    /// envelope per tool_call_id. Progress snapshots are coalesced
    /// latest-wins (replace in place, preserving seq) so long-running
    /// background tasks don't bloat the replay log. Safe because the
    /// event_log is append-only.
    progress_log_index: HashMap<String, usize>,
}

impl AgentReplayState {
    fn clear_active_stream(&mut self) {
        self.active_stream = None;
        self.completed_stream = None;
    }

    fn discard_active_stream(&mut self) {
        if let Some(stream) = self.active_stream.take() {
            self.terminal_stream_message_ids.insert(stream.message_id);
        }
        self.completed_stream = None;
    }

    fn active_stream_events(&self) -> Vec<ChatEvent> {
        let mut events = Vec::new();
        if self.typing {
            events.push(ChatEvent::TypingStatusChanged(true));
        }

        let Some(active) = &self.active_stream else {
            if self.typing {
                self.completed_stream_events(&mut events);
            }
            return events;
        };

        let current_message_id = active.message_id.0.clone();
        events.push(ChatEvent::StreamStart(active.start.clone()));
        if !active.reasoning.is_empty() {
            events.push(ChatEvent::StreamReasoningDelta(StreamTextDeltaData {
                message_id: Some(current_message_id.clone()),
                text: active.reasoning.clone(),
            }));
        }
        if !active.text.is_empty() {
            events.push(ChatEvent::StreamDelta(StreamTextDeltaData {
                message_id: Some(current_message_id),
                text: active.text.clone(),
            }));
        }
        events.extend(active.tool_events.iter().cloned());
        events
    }

    fn completed_stream_events(&self, events: &mut Vec<ChatEvent>) {
        let Some(completed) = &self.completed_stream else {
            return;
        };
        let current_message_id = completed.stream.message_id.0.clone();
        events.push(ChatEvent::StreamStart(completed.stream.start.clone()));
        if !completed.stream.reasoning.is_empty() {
            events.push(ChatEvent::StreamReasoningDelta(StreamTextDeltaData {
                message_id: Some(current_message_id.clone()),
                text: completed.stream.reasoning.clone(),
            }));
        }
        if !completed.stream.text.is_empty() {
            events.push(ChatEvent::StreamDelta(StreamTextDeltaData {
                message_id: Some(current_message_id),
                text: completed.stream.text.clone(),
            }));
        }
        events.extend(completed.stream.tool_events.iter().cloned());
        events.push(ChatEvent::StreamEnd(completed.end.clone()));
        events.extend(completed.post_end_events.iter().cloned());
    }

    fn active_completed_stream_history_filter(&self) -> Option<CompletedStreamHistoryFilter> {
        if !self.typing || self.active_stream.is_some() {
            return None;
        }
        let completed = self.completed_stream.as_ref()?;
        let mut tool_call_ids = completed
            .stream
            .tool_events
            .iter()
            .filter_map(chat_event_tool_call_id)
            .map(ToOwned::to_owned)
            .collect::<HashSet<_>>();
        tool_call_ids.extend(
            completed
                .post_end_events
                .iter()
                .filter_map(chat_event_tool_call_id)
                .map(ToOwned::to_owned),
        );
        let message = Some(completed.end.message.clone());
        Some(CompletedStreamHistoryFilter {
            message,
            tool_call_ids,
        })
    }

    fn update_completed_stream_metadata(&mut self, update: &MessageMetadataUpdateData) {
        if !self.typing || self.active_stream.is_some() {
            return;
        }
        let Some(completed) = self.completed_stream.as_mut() else {
            return;
        };
        if completed.end.message.message_id.as_ref() != Some(&update.message_id) {
            return;
        }
        if update.model_info.is_some() {
            completed.end.message.model_info = update.model_info.clone();
        }
        if update.token_usage.is_some() {
            completed.end.message.token_usage = update.token_usage.clone();
        }
        if update.context_breakdown.is_some() {
            completed.end.message.context_breakdown = update.context_breakdown.clone();
        }
    }

    fn update_completed_stream_tool_snapshot(&mut self, event: &ChatEvent) {
        if !self.typing || self.active_stream.is_some() {
            return;
        }
        let Some(completed) = self.completed_stream.as_mut() else {
            return;
        };
        let Some(tool_call_id) = chat_event_tool_call_id(event) else {
            return;
        };
        if upsert_tool_event(&mut completed.post_end_events, event) {
            return;
        }
        let belongs_to_completed_stream = completed
            .stream
            .tool_events
            .iter()
            .filter_map(chat_event_tool_call_id)
            .any(|existing_tool_call_id| existing_tool_call_id == tool_call_id);
        if belongs_to_completed_stream {
            if !upsert_tool_event(&mut completed.stream.tool_events, event) {
                completed.post_end_events.push(event.clone());
            }
            return;
        }
        completed.post_end_events.push(event.clone());
    }
}

struct CompletedStreamHistoryFilter {
    message: Option<ChatMessage>,
    tool_call_ids: HashSet<String>,
}

impl CompletedStreamHistoryFilter {
    fn matches(&self, event: &ChatEvent) -> bool {
        let completed_message_id = self
            .message
            .as_ref()
            .and_then(|message| message.message_id.as_ref())
            .map(|message_id| message_id.0.as_str());
        match event {
            ChatEvent::MessageAdded(message) => self
                .message
                .as_ref()
                .is_some_and(|completed_message| same_chat_message(completed_message, message)),
            ChatEvent::StreamStart(start) => start.message_id.as_deref() == completed_message_id,
            ChatEvent::StreamDelta(delta) | ChatEvent::StreamReasoningDelta(delta) => {
                delta.message_id.as_deref() == completed_message_id
            }
            ChatEvent::StreamEnd(end) => self.message.as_ref().is_some_and(|completed_message| {
                same_chat_message(completed_message, &end.message)
            }),
            ChatEvent::ToolRequest(_)
            | ChatEvent::ToolProgress(_)
            | ChatEvent::ToolExecutionCompleted(_) => chat_event_tool_call_id(event)
                .is_some_and(|tool_call_id| self.tool_call_ids.contains(tool_call_id)),
            _ => false,
        }
    }
}

struct ReplayActiveStream {
    message_id: ChatMessageId,
    start: StreamStartData,
    text: String,
    reasoning: String,
    tool_events: Vec<ChatEvent>,
}

struct ReplayCompletedStream {
    stream: ReplayActiveStream,
    end: StreamEndData,
    post_end_events: Vec<ChatEvent>,
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
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum TokenUsageTrackingMode {
    #[default]
    Messages,
    ModelRequests,
}

#[derive(Debug, Default)]
struct AgentActivityStatsTracker {
    stats: AgentActivityStats,
    seen_tool_calls: HashSet<String>,
    token_usage_by_source: HashMap<TokenUsageSource, TaskTokenUsageScope>,
    active_reasoning: String,
    latest_model: Option<String>,
    token_usage_tracking_mode: TokenUsageTrackingMode,
}

impl AgentActivityStatsTracker {
    fn for_backend(backend_kind: BackendKind) -> Self {
        Self {
            token_usage_tracking_mode: if backend_kind == BackendKind::Codex {
                TokenUsageTrackingMode::ModelRequests
            } else {
                TokenUsageTrackingMode::Messages
            },
            ..Self::default()
        }
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
            | ChatEvent::ToolProgress(_)
            | ChatEvent::ToolExecutionCompleted(_)
            | ChatEvent::TaskUpdate(_)
            | ChatEvent::OperationCancelled(_)
            | ChatEvent::RetryAttempt(_)
            | ChatEvent::Orchestration(_) => {}
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
        self.stats.source_through_seq = Some(source_seq);
        self.stats != previous
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
            let reason = match token_usage.turn {
                TokenUsageScope::Known { .. } => TaskTokenUsageUnavailableReason::AgentUnavailable,
                TokenUsageScope::Unavailable { reason } => {
                    task_token_usage_reason_from_message_reason(reason)
                }
            };
            self.token_usage_by_source
                .insert(source, TaskTokenUsageScope::Unavailable { reason });
            return token_usage;
        };

        self.token_usage_by_source.insert(
            source,
            TaskTokenUsageScope::Known {
                usage: Box::new(TaskTokenUsageAmount::from_token_usage(&turn_usage)),
            },
        );
        if let Some(cumulative) = token_usage.cumulative.known_usage().cloned() {
            self.stats.token_usage = cumulative;
            self.token_usage_by_source
                .retain(|_, usage| matches!(usage, TaskTokenUsageScope::Known { .. }));
        } else {
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
        self.stats.source_through_seq = Some(source_seq);
        token_usage
    }

    fn refresh_token_usage(&mut self) {
        self.stats.token_usage = total_task_token_usage(
            self.token_usage_by_source
                .values()
                .filter_map(|usage| usage.reported_usage()),
        );
    }
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

fn total_task_token_usage<'a>(
    entries: impl Iterator<Item = &'a TaskTokenUsageAmount>,
) -> TokenUsage {
    let mut total = TokenUsage::default();
    for usage in entries {
        total.input_tokens = total
            .input_tokens
            .saturating_add(usage.input_tokens.unwrap_or(0));
        total.output_tokens = total
            .output_tokens
            .saturating_add(usage.output_tokens.unwrap_or(0));
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

    pub async fn read_supervision_context(&self) -> Option<supervisor::SupervisionContextSnapshot> {
        let (reply_tx, reply_rx) = oneshot::channel();
        if self
            .tx
            .send(AgentCommand::ReadSupervisionContext { reply: reply_tx })
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
        self.closing.store(true, Ordering::SeqCst);
        self.accepting_input.store(false, Ordering::SeqCst);
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
fn startup_completion_test_gates() -> &'static StartupCompletionTestGates {
    static GATES: std::sync::OnceLock<StartupCompletionTestGates> = std::sync::OnceLock::new();
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
async fn wait_for_startup_completion_test_gate(agent_name: &str) {
    let gate = startup_completion_test_gates()
        .lock()
        .expect("startup completion test gate mutex poisoned")
        .get(agent_name)
        .cloned();
    if let Some(gate) = gate {
        crate::host::wait_for_spawn_operation_test_gate_inner(&gate).await;
        startup_completion_test_gates()
            .lock()
            .expect("startup completion test gate mutex poisoned")
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

enum ActorLifecycle {
    Running,
    Closing,
}

pub(crate) struct GenerateAgentNameRequest {
    pub backend_kind: BackendKind,
    pub prompt: String,
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
    pub use_mock_backend: bool,
    pub capacity_tx: HostCapacityTx,
}

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
    let spawn_config = agent_name_generation_spawn_config();
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

pub(crate) fn agent_name_generation_spawn_config() -> BackendSpawnConfig {
    BackendSpawnConfig {
        execution_mode: BackendExecutionMode::InferenceOnly,
        cost_hint: Some(SpawnCostHint::Low),
        custom_agent_id: None,
        startup_mcp_servers: Vec::new(),
        session_settings: None,
        backend_config: Default::default(),
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
    let resolved_spawn_config = crate::agent::customization::ResolvedSpawnConfig {
        access_mode: BackendAccessMode::ReadOnly,
        ..Default::default()
    };
    let spawn_config = BackendSpawnConfig {
        execution_mode: BackendExecutionMode::Agent,
        cost_hint: Some(SpawnCostHint::Low),
        custom_agent_id: None,
        startup_mcp_servers: Vec::new(),
        session_settings: None,
        backend_config: Default::default(),
        resolved_spawn_config,
    };
    let initial_input = SendMessagePayload {
        message: prompt,
        images: None,
        origin: None,
        tool_response: None,
    };
    let (host_sub_agent_spawn_tx, _host_sub_agent_spawn_rx) = mpsc::unbounded_channel();
    // Some backends require a real workspace root at spawn time. The helper
    // keeps the summarized agent's roots but remains read-only and has no MCP
    // servers, so it can read context without write/tool side effects.
    let workspace_roots = request.workspace_roots.clone();
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
                let tool_name = requested_tool.tool_name;
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
trait BackendSender: Send + 'static {
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
}

impl<B: Backend> BackendSender for B {
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
        BackendKind::Kiro => {
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
        BackendKind::Kiro => {
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
        BackendKind::Kiro => {
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
) -> BackendFuture<BackendSpawnResult> {
    Box::pin(async move {
        let (b, events) =
            MockBackend::spawn(workspace_roots.clone(), config, initial_input).await?;
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
) -> BackendFuture<BackendResumeResult> {
    Box::pin(async move {
        let (b, events) = MockBackend::resume(
            workspace_roots.clone(),
            BackendSpawnConfig::default(),
            session_id.clone(),
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
) -> BackendFuture<BackendForkResult> {
    Box::pin(async move {
        let (b, events) = MockBackend::fork(
            workspace_roots.clone(),
            config,
            from_session_id,
            initial_input,
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
        host_sub_agent_spawn_tx,
        capacity_tx,
        review_registry,
        status_handle,
        antigravity_conversations_dir,
    } = runtime;
    let sub_agent_context = HostSubAgentEmitterContext {
        host_sub_agent_spawn_tx,
        capacity_tx,
    };
    let (tx, mut rx) = mpsc::unbounded_channel::<AgentCommand>();
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
            startup_mcp_servers,
            resolved_spawn_config,
            resume_session_id,
            fork_from_session_id,
            startup_warning,
            startup_failure,
            initial_alias,
            use_mock_backend,
            ..
        } = request;
        let mut current_start = start.clone();
        let spawn_config = BackendSpawnConfig {
            execution_mode: BackendExecutionMode::Agent,
            cost_hint,
            custom_agent_id: current_start.custom_agent_id.clone(),
            startup_mcp_servers,
            session_settings,
            backend_config: backend_config.clone(),
            resolved_spawn_config: resolved_spawn_config.clone(),
        };
        let initial_cost_hint = spawn_config.cost_hint;
        let initial_session_settings = spawn_config.session_settings.clone();
        let canonical_stream = format!("/agent/{}", agent_id);
        let mut event_log: Vec<Envelope> = Vec::new();
        let mut latest_output = AgentControlLatestOutput::default();
        let mut replay_state = AgentReplayState::default();
        let mut last_stream_identity_violation: Option<StreamIdentityViolation> = None;
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
                startup_mcp_servers: Vec::new(),
                session_settings: initial_session_settings,
                backend_config,
                resolved_spawn_config,
            },
        );
        let mut queue = VecDeque::new();
        let mut pending_inputs: VecDeque<AgentInput> = VecDeque::new();
        let mut pending_name_commands = VecDeque::new();
        assert!(
            resume_session_id.is_none() || fork_from_session_id.is_none(),
            "spawn request cannot both resume and fork a session"
        );
        let starts_with_initial_turn = resume_session_id.is_none();
        let is_resume = resume_session_id.is_some();

        #[cfg(feature = "test-support")]
        let startup_gate_name = current_start.name.clone();
        let mut startup_future = Box::pin(async {
            #[cfg(feature = "test-support")]
            wait_for_startup_completion_test_gate(&startup_gate_name).await;
            #[cfg(test)]
            wait_for_agent_startup_test_gate(&agent_id).await;
            let startup_result: Result<
                (
                    BackendHandle,
                    EventStream,
                    SessionId,
                    Option<SendMessagePayload>,
                ),
                AgentStartupFailure,
            > = if let Some(err) = startup_failure {
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
            startup_result
        });
        let startup_cancellation_supported = backend_startup_drop_cancels_workers(backend_kind);
        let mut pending_startup_attaches: Vec<(Stream, oneshot::Sender<bool>)> = Vec::new();
        #[cfg(test)]
        wait_for_agent_startup_selection_test_gate(&agent_id).await;
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
                    let Some(command) = command else {
                        return;
                    };
                    match command {
                        AgentCommand::Interrupt { reply } => {
                            let _ = reply.send(InterruptOutcome::Interrupted);
                            break Err(AgentStartupFailure::internal("agent startup interrupted"));
                        }
                        AgentCommand::Close { reply } => {
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
                        AgentCommand::ReadSupervisionContext { reply } => {
                            let _ = reply.send(supervisor::SupervisionContextSnapshot::default());
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
                        AgentCommand::ReleaseCompaction { reply } => {
                            let _ = reply.send(());
                        }
                        AgentCommand::SendInput(input) => {
                            pending_inputs.push_back(input);
                        }
                        AgentCommand::ResumeReplayBarrier { .. } => {}
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
                );
                park_terminal_agent(
                    &session_store,
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
        let mut lifecycle = ActorLifecycle::Running;
        let mut close_reply: Option<oneshot::Sender<()>> = None;
        let mut active_compaction: Option<ActiveCompaction> = None;
        let mut compaction_blocked = false;
        current_session_id = Some(actor_session_id.clone());
        current_start.session_id = Some(actor_session_id.clone());
        let _ = start_tx.send(current_start.clone());
        let mut resume_replay_gate_pending = false;
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
        status_handle
            .update(|s| {
                record_agent_started(s, is_resume);
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
        )
        .await;
        if !resume_replay_gate_pending {
            flush_pending_agent_attaches(
                &event_log,
                Some(&replay_state),
                &mut latest_output,
                &mut subscribers,
                &mut pending_startup_attaches,
            );
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
                    pending_inputs: &mut pending_inputs,
                    rx: &mut rx,
                },
            )
            .await
        {
            abort_resume_replay_barrier_task(&mut resume_replay_barrier_task);
            return;
        }
        loop {
            latest_output
                .observe_event_log(&event_log)
                .expect("typed agent replay log must project latest output");
            tokio::select! {
                maybe_event = events.recv_backend() => {
                    let Some(event) = maybe_event else {
                        if let Some(compaction) = active_compaction.take() {
                            let _ = compaction
                                .reply
                                .send(Err("agent backend closed during compaction".to_owned()));
                        }
                        if resume_replay_gate_pending {
                            let payload = AgentErrorPayload {
                                agent_id: current_start.agent_id.clone(),
                                code: AgentErrorCode::BackendFailed,
                                message: "agent backend closed before resume replay completed"
                                    .to_owned(),
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
                            );
                            abort_resume_replay_barrier_task(&mut resume_replay_barrier_task);
                            if let Some(backend) = backend.take() {
                                shutdown_backend_with_timeout(backend, &current_start.agent_id)
                                    .await;
                            }
                            park_terminal_agent(
                                &session_store,
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
                                shutdown_backend_with_timeout(backend, &current_start.agent_id).await;
                            }
                            abort_resume_replay_barrier_task(&mut resume_replay_barrier_task);
                            finish_actor_close(&accepting_input_task, &status_handle, reply).await;
                            return;
                        }
                        let payload = AgentErrorPayload {
                            agent_id: current_start.agent_id.clone(),
                            code: AgentErrorCode::BackendFailed,
                            message: "agent backend closed".to_owned(),
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
                            },
                            &payload,
                        )
                        .await;
                        park_terminal_agent(
                            &session_store,
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
                    };
                    if let Err(violation) =
                        validate_chat_event_stream_identity(&replay_state, &event)
                    {
                        if last_stream_identity_violation != Some(violation) {
                            last_stream_identity_violation = Some(violation);
                            let error = stream_identity_violation_event(violation);
                            append_chat_event(
                                &canonical_stream,
                                &mut event_log,
                                &mut subscribers,
                                &mut replay_state,
                                &error,
                            )
                            .await;
                        }
                        match recover_stream_identity_violation(
                            &replay_state,
                            &mut event,
                            violation,
                        ) {
                            StreamIdentityRecovery::Resync { finalize_abandoned } => {
                                if let Some(finalize) = finalize_abandoned {
                                    append_chat_event(
                                        &canonical_stream,
                                        &mut event_log,
                                        &mut subscribers,
                                        &mut replay_state,
                                        &finalize,
                                    )
                                    .await;
                                }
                                if validate_chat_event_stream_identity(&replay_state, &event)
                                    .is_err()
                                {
                                    continue;
                                }
                            }
                            StreamIdentityRecovery::Unrecoverable => continue,
                        }
                    } else {
                        last_stream_identity_violation = None;
                    }
                    if resume_replay_gate_pending {
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
                        continue;
                    }
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
                            status_handle.update(|s| {
                                s.is_thinking = typing;
                                if completed_by_idle {
                                    s.turn_completed = true;
                                }
                                s.activity_counter = s.activity_counter.saturating_add(1);
                            }).await;
                        }
                        ChatEvent::OperationCancelled(_) => {
                            pending_tool_response_ids.clear();
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
                            let waiting_for_plan_approval = matches!(
                                &request.tool_type,
                                protocol::ToolRequestType::ExitPlanMode { .. }
                            );
                            if waiting_for_plan_approval {
                                pending_tool_response_ids.insert(request.tool_call_id.clone());
                                in_turn = true;
                                idle_transition_armed = false;
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
                                    tool_name = %completion.tool_name,
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
                                    s.last_error = completion.error.clone();
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
                    apply_runtime_session_updates(
                        &session_store,
                        current_session_id
                            .as_ref()
                            .expect("live agent must have session_id"),
                        &event,
                    )
                    .await;
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
                        shutdown_backend_with_timeout(backend, &current_start.agent_id).await;
                        abort_resume_replay_barrier_task(&mut resume_replay_barrier_task);
                        finish_actor_close(&accepting_input_task, &status_handle, reply).await;
                        return;
                    }

                    if real_idle_transition
                        && matches!(lifecycle, ActorLifecycle::Running)
                        && !compaction_blocked
                        && !queue.is_empty()
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
                        )
                        .await;
                        in_turn = true;
                        idle_transition_armed = false;
                        let outcome = backend
                            .as_ref()
                            .expect("backend must exist while actor is running")
                            .send_with_outcome(AgentInput::SendMessage(
                                queued_message_to_send_payload(queued.clone()),
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
                                enter_terminal_failure(
                                    TerminalFailureContext {
                                        accepting_input: &accepting_input_task,
                                        status_handle: &status_handle,
                                        canonical_stream: &canonical_stream,
                                        event_log: &mut event_log,
                                        replay_state: &mut replay_state,
                                        subscribers: &mut subscribers,
                                        queue: &mut queue,
                                    },
                                    &payload,
                                )
                                .await;
                                park_terminal_agent(
                                    &session_store,
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
                                if let Some(MessageOrigin::Review { review_id }) = queued.origin {
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
                    match command {
                        AgentCommand::ResumeReplayBarrier { result } => {
                            if !resume_replay_gate_pending {
                                continue;
                            }
                            // Drain any replay events already buffered on the
                            // backend stream before closing the gate. The
                            // select! is unbiased, so the barrier command can be
                            // selected while replay events are still queued;
                            // ingesting them here (rather than leaving them for a
                            // now-ungated `events.recv()`) keeps the full resume
                            // transcript off the live broadcast path.
                            while let Ok(event) = events.try_recv_backend() {
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
                                }
                            }
                            if result.is_ok()
                                && let Some(task_list) = persisted_resume_task_list.take()
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
                                    accepting_input_task.store(true, Ordering::SeqCst);
                                    if initial_follow_up.is_none() {
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
                                    );
                                    if let Some(input) = initial_follow_up.take()
                                        && !send_initial_follow_up_or_park(
                                            input,
                                            InitialFollowUpContext {
                                                backend: &mut backend,
                                                in_turn: &mut in_turn,
                                                idle_transition_armed: &mut idle_transition_armed,
                                                session_store: &session_store,
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
                                    enter_terminal_failure(
                                        TerminalFailureContext {
                                            accepting_input: &accepting_input_task,
                                            status_handle: &status_handle,
                                            canonical_stream: &canonical_stream,
                                            event_log: &mut event_log,
                                            replay_state: &mut replay_state,
                                            subscribers: &mut subscribers,
                                            queue: &mut queue,
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
                                    );
                                    if let Some(backend) = backend.take() {
                                        shutdown_backend_with_timeout(
                                            backend,
                                            &current_start.agent_id,
                                        )
                                        .await;
                                    }
                                    park_terminal_agent(
                                        &session_store,
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
                        AgentCommand::SendInput(input) => {
                            if resume_replay_gate_pending {
                                pending_inputs.push_back(input);
                                continue;
                            }
                            if matches!(lifecycle, ActorLifecycle::Closing) {
                                continue;
                            }
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
                                continue;
                            }
                            match input {
                                AgentInput::SendMessage(msg) => {
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
                                    let is_tool_response = msg.tool_response.is_some();
                                    let plan_response = match msg.tool_response.as_ref() {
                                        Some(protocol::SendMessageToolResponse::ExitPlanMode {
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
                                    if in_turn && !is_tool_response {
                                        let queued_message_id =
                                            QueuedMessageId(Uuid::new_v4().to_string());
                                        queue.push_back(QueuedMessageEntry {
                                            id: queued_message_id.clone(),
                                            message: msg.message,
                                            images: msg.images.unwrap_or_default(),
                                            origin: msg.origin,
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
                                        )
                                        .await;
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
                                                    queue.push_front(
                                                        queued_entry_from_send_payload(payload),
                                                    );
                                                    update_queued_messages_snapshot(
                                                        &canonical_stream,
                                                        &mut event_log,
                                                        &mut subscribers,
                                                        &queue,
                                                    )
                                                    .await;
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
                                            let payload = AgentErrorPayload {
                                                agent_id: current_start.agent_id.clone(),
                                                code: AgentErrorCode::Internal,
                                                message: "agent backend closed".to_owned(),
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
                                                },
                                                &payload,
                                            )
                                            .await;
                                            park_terminal_agent(
                                                &session_store,
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
                                        if let Some(clear_pending_response) = plan_response {
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
                                    )
                                    .await;
                                    in_turn = true;
                                    idle_transition_armed = false;
                                    let outcome = backend
                                        .as_ref()
                                        .expect("backend must exist while actor is running")
                                        .send_with_outcome(AgentInput::SendMessage(
                                            queued_message_to_send_payload(queued.clone()),
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
                                            enter_terminal_failure(
                                                TerminalFailureContext {
                                                    accepting_input: &accepting_input_task,
                                                    status_handle: &status_handle,
                                                    canonical_stream: &canonical_stream,
                                                    event_log: &mut event_log,
                                                    replay_state: &mut replay_state,
                                                    subscribers: &mut subscribers,
                                                    queue: &mut queue,
                                                },
                                                &payload,
                                            )
                                            .await;
                                            park_terminal_agent(
                                                &session_store,
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
                                            if let Some(MessageOrigin::Review { review_id }) =
                                                queued.origin
                                            {
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
                                        continue;
                                    }
                                    if let Err(err) = validate_runtime_session_settings_update(
                                        current_start.backend_kind,
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
                                        continue;
                                    }
                                    if let Err(err) = backend
                                        .as_mut()
                                        .expect("backend must exist while actor is running")
                                        .update_session_settings(update)
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
                        AgentCommand::CompactIfInactive {
                            expected_activity_counter,
                            summary_prompt,
                            max_summary_bytes,
                            accepted,
                            reply,
                        } => {
                            let live_activity_counter =
                                status_handle.snapshot().await.activity_counter;
                            let reject = if live_activity_counter != expected_activity_counter {
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
                                .send_with_outcome(AgentInput::SendMessage(SendMessagePayload {
                                    message: summary_prompt,
                                    images: None,
                                    origin: Some(MessageOrigin::User),
                                    tool_response: None,
                                }))
                                .await;
                            if !matches!(outcome, SendOutcome::Accepted) {
                                let error = match outcome {
                                    SendOutcome::Busy(_) => {
                                        "agent backend rejected the compaction summary because it is busy"
                                    }
                                    SendOutcome::Closed => {
                                        "agent backend closed before compaction could start"
                                    }
                                    SendOutcome::Accepted => unreachable!(),
                                };
                                finish_active_compaction_with_error(
                                    &mut active_compaction,
                                    error.to_owned(),
                                );
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
                                .send_with_outcome(AgentInput::SendMessage(SendMessagePayload {
                                    message: summary_prompt,
                                    images: None,
                                    origin: None,
                                    tool_response: None,
                                }))
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
                            let _ =
                                reply.send(session_history_window(&event_log, before_seq, limit, Some(&replay_state)));
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
                        AgentCommand::ReadSupervisionContext { reply } => {
                            let _ = reply
                                .send(supervisor::supervision_context_snapshot(&event_log));
                        }
                        AgentCommand::ReadUsageSnapshot { reply } => {
                            let _ = reply.send(agent_usage_snapshot_from_tracker(
                                &current_start,
                                &activity_stats,
                            ));
                        }
                        AgentCommand::Interrupt { reply } => {
                            if matches!(lifecycle, ActorLifecycle::Closing) {
                                let _ = reply.send(InterruptOutcome::NotRunning);
                                continue;
                            }
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
                        AgentCommand::Close { reply } => {
                            accepting_input_task.store(false, Ordering::SeqCst);
                            if matches!(lifecycle, ActorLifecycle::Closing) {
                                let _ = reply.send(());
                                continue;
                            }
                            lifecycle = ActorLifecycle::Closing;
                            close_reply = Some(reply);
                            if !queue.is_empty() {
                                queue.clear();
                                update_queued_messages_snapshot(
                                    &canonical_stream,
                                    &mut event_log,
                                    &mut subscribers,
                                    &queue,
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
                                shutdown_backend_with_timeout(backend, &current_start.agent_id).await;
                                abort_resume_replay_barrier_task(&mut resume_replay_barrier_task);
                                finish_actor_close(&accepting_input_task, &status_handle, reply).await;
                                return;
                            }
                        }
                        AgentCommand::Attach { stream, reply } => {
                            if resume_replay_gate_pending {
                                pending_resume_attaches.push((stream, reply));
                                continue;
                            }
                            let attached = attach_subscriber_with_latest_output(
                                &event_log,
                                Some(&replay_state),
                                latest_output.output(),
                                &mut subscribers,
                                stream,
                            );
                            let _ = reply.send(attached);
                        }
                    }
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
    Command(Option<AgentCommand>),
}

fn backend_startup_drop_cancels_workers(backend_kind: BackendKind) -> bool {
    // Enabling the command race is safe only when every startup path for the
    // backend explicitly cancels or reaps work after its returned future drops.
    matches!(
        backend_kind,
        BackendKind::Claude | BackendKind::Codex | BackendKind::Tycode
    )
}

#[cfg(test)]
struct AgentStartupTestGate {
    agent_id: AgentId,
    entered: oneshot::Sender<()>,
    release: oneshot::Receiver<()>,
}

#[cfg(test)]
static AGENT_STARTUP_TEST_GATE: std::sync::Mutex<Option<AgentStartupTestGate>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
static AGENT_STARTUP_SELECTION_TEST_GATE: std::sync::Mutex<Option<AgentStartupTestGate>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
async fn wait_for_agent_startup_test_gate(agent_id: &AgentId) {
    wait_for_matching_agent_startup_test_gate(&AGENT_STARTUP_TEST_GATE, agent_id).await;
}

#[cfg(test)]
async fn wait_for_agent_startup_selection_test_gate(agent_id: &AgentId) {
    wait_for_matching_agent_startup_test_gate(&AGENT_STARTUP_SELECTION_TEST_GATE, agent_id).await;
}

#[cfg(test)]
async fn wait_for_matching_agent_startup_test_gate(
    slot: &std::sync::Mutex<Option<AgentStartupTestGate>>,
    agent_id: &AgentId,
) {
    let gate = {
        let mut gate = slot.lock().expect("agent startup test gate mutex poisoned");
        if gate.as_ref().is_some_and(|gate| &gate.agent_id == agent_id) {
            gate.take()
        } else {
            None
        }
    };
    if let Some(gate) = gate {
        let _ = gate.entered.send(());
        let _ = gate.release.await;
    }
}

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
        command = rx.recv(), if cancellation_supported => AgentStartupEvent::Command(command),
        result = startup => AgentStartupEvent::Completed(result),
    }
}

pub(crate) struct RelayEventReceivers {
    pub events: mpsc::UnboundedReceiver<ChatEvent>,
    pub model_usage: mpsc::UnboundedReceiver<ModelRequestTokenUsage>,
    pub total_usage: mpsc::UnboundedReceiver<u64>,
}

pub(crate) fn spawn_relay_agent_actor(
    agent_id: AgentId,
    start: AgentStartPayload,
    receivers: RelayEventReceivers,
    session_store: Arc<Mutex<SessionStore>>,
    session_id: SessionId,
    status_handle: registry::AgentStatusHandle,
) -> AgentHandle {
    let RelayEventReceivers {
        mut events,
        mut model_usage,
        mut total_usage,
    } = receivers;
    let (tx, mut rx) = mpsc::unbounded_channel::<AgentCommand>();
    let accepting_input = Arc::new(AtomicBool::new(true));
    let accepting_input_task = Arc::clone(&accepting_input);
    let closing = Arc::new(AtomicBool::new(false));
    let (start_tx, start_rx) = watch::channel(start.clone());

    tokio::spawn(async move {
        let canonical_stream = format!("/agent/{}", agent_id);
        let mut event_log: Vec<Envelope> = Vec::new();
        let mut latest_output = AgentControlLatestOutput::default();
        let mut replay_state = AgentReplayState::default();
        let mut last_stream_identity_violation: Option<StreamIdentityViolation> = None;
        let mut subscribers: Vec<Stream> = Vec::new();
        let mut active_stream_text = String::new();
        let mut activity_stats = AgentActivityStatsTracker::for_backend(start.backend_kind);
        let mut activity_event_seq = 0_u64;
        let mut current_start = start;
        let mut pending_alias = None;
        let mut in_turn = false;
        let mut pending_tool_response_ids: HashSet<String> = HashSet::new();
        let mut lifecycle = ActorLifecycle::Running;
        let mut close_reply: Option<oneshot::Sender<()>> = None;
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
                            finish_actor_close(&accepting_input_task, &status_handle, reply).await;
                            return;
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

                    if let Err(violation) =
                        validate_chat_event_stream_identity(&replay_state, &event)
                    {
                        if last_stream_identity_violation != Some(violation) {
                            last_stream_identity_violation = Some(violation);
                            let error = stream_identity_violation_event(violation);
                            append_chat_event(
                                &canonical_stream,
                                &mut event_log,
                                &mut subscribers,
                                &mut replay_state,
                                &error,
                            )
                            .await;
                        }
                        match recover_stream_identity_violation(
                            &replay_state,
                            &mut event,
                            violation,
                        ) {
                            StreamIdentityRecovery::Resync { finalize_abandoned } => {
                                if let Some(finalize) = finalize_abandoned {
                                    append_chat_event(
                                        &canonical_stream,
                                        &mut event_log,
                                        &mut subscribers,
                                        &mut replay_state,
                                        &finalize,
                                    )
                                    .await;
                                }
                                if validate_chat_event_stream_identity(&replay_state, &event)
                                    .is_err()
                                {
                                    continue;
                                }
                            }
                            StreamIdentityRecovery::Unrecoverable => continue,
                        }
                    } else {
                        last_stream_identity_violation = None;
                    }

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

                    apply_runtime_session_updates(&session_store, &session_id, &event).await;
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
                            let _ =
                                reply.send(session_history_window(&event_log, before_seq, limit, Some(&replay_state)));
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
                        AgentCommand::ReadSupervisionContext { reply } => {
                            let _ = reply
                                .send(supervisor::supervision_context_snapshot(&event_log));
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
                                finish_actor_close(&accepting_input_task, &status_handle, reply).await;
                                return;
                            }
                        }
                        AgentCommand::Attach { stream, reply } => {
                            let attached = attach_subscriber_with_latest_output(
                                &event_log,
                                Some(&replay_state),
                                latest_output.output(),
                                &mut subscribers,
                                stream,
                            );
                            let _ = reply.send(attached);
                        }
                    }
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

async fn shutdown_backend_with_timeout(backend: BackendHandle, agent_id: &AgentId) {
    if tokio::time::timeout(BACKEND_SHUTDOWN_TIMEOUT, backend.shutdown())
        .await
        .is_err()
    {
        tracing::error!(
            agent_id = %agent_id,
            timeout_ms = BACKEND_SHUTDOWN_TIMEOUT.as_millis(),
            "timed out shutting down backend"
        );
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

fn compaction_input_rejected_payload(agent_id: &AgentId) -> AgentErrorPayload {
    AgentErrorPayload {
        agent_id: agent_id.clone(),
        code: AgentErrorCode::Internal,
        message: "agent compaction is in progress".to_owned(),
        fatal: false,
    }
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

async fn enter_terminal_failure(context: TerminalFailureContext<'_>, payload: &AgentErrorPayload) {
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
                let _ = reply.send(session_history_window(event_log, before_seq, limit, None));
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
            AgentCommand::ReadSupervisionContext { reply } => {
                let _ = reply.send(supervisor::supervision_context_snapshot(event_log));
            }
            AgentCommand::ReadUsageSnapshot { reply } => {
                let _ = reply.send(agent_usage_snapshot_from_log(current_start, event_log));
            }
            AgentCommand::Attach { stream, reply } => {
                let attached = attach_subscriber_with_latest_output(
                    event_log,
                    None,
                    latest_output.output(),
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
            AgentCommand::CompactIfInactive { accepted, reply, .. } => {
                let error = "agent is not running".to_owned();
                let _ = accepted.send(Err(error.clone()));
                let _ = reply.send(Err(error));
            }
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
            AgentCommand::Interrupt { reply } => {
                let _ = reply.send(InterruptOutcome::NotRunning);
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn park_relay_terminal_agent(
    session_store: &Arc<Mutex<SessionStore>>,
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
                let _ = reply.send(session_history_window(event_log, before_seq, limit, None));
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
            AgentCommand::ReadSupervisionContext { reply } => {
                let _ = reply.send(supervisor::supervision_context_snapshot(event_log));
            }
            AgentCommand::ReadUsageSnapshot { reply } => {
                let _ = reply.send(agent_usage_snapshot_from_log(current_start, event_log));
            }
            AgentCommand::Attach { stream, reply } => {
                let attached = attach_subscriber_with_latest_output(
                    event_log,
                    None,
                    latest_output.output(),
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
            AgentCommand::CompactIfInactive { accepted, reply, .. } => {
                let error = "backend-native agents cannot be compacted".to_owned();
                let _ = accepted.send(Err(error.clone()));
                let _ = reply.send(Err(error));
            }
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
            && backend_session_is_resumable(current_start.backend_kind, session_id),
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

fn backend_session_is_resumable(backend_kind: BackendKind, session_id: &SessionId) -> bool {
    match backend_kind {
        BackendKind::Antigravity => is_antigravity_native_session_id(session_id),
        BackendKind::Tycode
        | BackendKind::Kiro
        | BackendKind::Claude
        | BackendKind::Codex
        | BackendKind::Hermes => true,
    }
}

fn interrupted_tool_completion(completion: &ToolExecutionCompletedData) -> bool {
    const CLAUDE_MISSING_TOOL_RESULT: &str = "history did not contain a tool_result";
    const TOOL_INTERRUPTED: &str = "Tool execution was interrupted";

    if completion.success {
        return false;
    }

    if completion
        .error
        .as_deref()
        .is_some_and(|error| error.contains(CLAUDE_MISSING_TOOL_RESULT))
    {
        return true;
    }

    match &completion.tool_result {
        ToolExecutionResult::Cancelled { .. } => false,
        ToolExecutionResult::Error {
            short_message,
            detailed_message,
        } => {
            short_message == TOOL_INTERRUPTED
                && detailed_message.contains(CLAUDE_MISSING_TOOL_RESULT)
        }
        ToolExecutionResult::RunCommand { .. } => false,
        ToolExecutionResult::ModifyFile { .. }
        | ToolExecutionResult::ReadFiles { .. }
        | ToolExecutionResult::SearchTypes { .. }
        | ToolExecutionResult::GetTypeDocs { .. }
        | ToolExecutionResult::TydeSendAgentMessage
        | ToolExecutionResult::TydeAwaitAgents { .. }
        | ToolExecutionResult::GenerateImage { .. }
        | ToolExecutionResult::WebSearch
        | ToolExecutionResult::ViewImage
        | ToolExecutionResult::Sleep
        | ToolExecutionResult::Other { .. } => false,
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
    if let Err(violation) = validate_chat_event_stream_identity(replay_state, event) {
        let error = stream_identity_violation_event(violation);
        record_chat_event_for_replay(canonical_stream, event_log, replay_state, &error)
            .expect("identity violation error is a non-stream event");
        broadcast_live_event(subscribers, FrameKind::ChatEvent, &error).await;
        return;
    }
    record_chat_event_for_replay(canonical_stream, event_log, replay_state, event)
        .expect("preflighted replay event remains valid");
    broadcast_live_event(subscribers, FrameKind::ChatEvent, event).await;
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

fn flush_pending_agent_attaches(
    event_log: &[Envelope],
    replay_state: Option<&AgentReplayState>,
    latest_output: &mut AgentControlLatestOutput,
    subscribers: &mut Vec<Stream>,
    pending_attaches: &mut Vec<(Stream, oneshot::Sender<bool>)>,
) {
    let output = current_latest_output(latest_output, event_log)
        .expect("typed agent replay log must project latest output");
    for (stream, reply) in std::mem::take(pending_attaches) {
        let attached = attach_subscriber_with_latest_output(
            event_log,
            replay_state,
            &output,
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
            mark_agent_turn_active(context.status_handle).await;
            return true;
        }
        SendOutcome::Busy(input) => {
            if let AgentInput::SendMessage(payload) = input {
                tracing::info!(
                    agent_id = %context.current_start.agent_id,
                    "backend busy with a self-started turn; initial follow-up queued at front"
                );
                context
                    .queue
                    .push_front(queued_entry_from_send_payload(payload));
                update_queued_messages_snapshot(
                    context.canonical_stream,
                    context.event_log,
                    context.subscribers,
                    context.queue,
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
        },
        &payload,
    )
    .await;
    park_terminal_agent(
        context.session_store,
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

async fn mark_agent_turn_active(status_handle: &registry::AgentStatusHandle) {
    status_handle
        .update(|status| {
            status.is_thinking = true;
            status.turn_completed = false;
            status.last_error = None;
            status.activity_counter = status.activity_counter.saturating_add(1);
        })
        .await;
}

fn record_agent_started(status: &mut registry::AgentStatus, is_resume: bool) {
    status.started = true;
    if is_resume {
        status.is_thinking = false;
        status.turn_completed = true;
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
    if let Err(violation) = validate_chat_event_stream_identity(replay_state, event) {
        let error = stream_identity_violation_event(violation);
        record_chat_event_for_replay(canonical_stream, event_log, replay_state, &error)
            .expect("identity violation error is a non-stream event");
        return;
    }
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
    record_chat_event_for_replay(canonical_stream, event_log, replay_state, &*event)
        .expect("preflighted replay event remains valid");
}

fn record_chat_event_for_replay(
    canonical_stream: &str,
    event_log: &mut Vec<Envelope>,
    replay_state: &mut AgentReplayState,
    event: &ChatEvent,
) -> Result<(), StreamIdentityViolation> {
    validate_chat_event_stream_identity(replay_state, event)?;
    match event {
        ChatEvent::StreamStart(start) => {
            let message_id = required_replay_stream_message_id(start.required_message_id())?;
            replay_state.completed_stream = None;
            replay_state.active_stream = Some(ReplayActiveStream {
                message_id,
                start: start.clone(),
                text: String::new(),
                reasoning: String::new(),
                tool_events: Vec::new(),
            });
        }
        ChatEvent::StreamDelta(delta) => {
            let message_id = required_replay_stream_message_id(delta.required_message_id())?;
            let Some(active) = replay_state.active_stream.as_mut() else {
                return Err(StreamIdentityViolation::ForeignActiveMessageId);
            };
            if message_id != active.message_id {
                return Err(StreamIdentityViolation::ForeignActiveMessageId);
            }
            active.text.push_str(&delta.text);
        }
        ChatEvent::StreamReasoningDelta(delta) => {
            let message_id = required_replay_stream_message_id(delta.required_message_id())?;
            let Some(active) = replay_state.active_stream.as_mut() else {
                return Err(StreamIdentityViolation::ForeignActiveMessageId);
            };
            if message_id != active.message_id {
                return Err(StreamIdentityViolation::ForeignActiveMessageId);
            }
            active.reasoning.push_str(&delta.text);
        }
        ChatEvent::StreamEnd(data) => {
            let message_id = required_replay_chat_message_id(data.required_message_id())?;
            let stream = replay_state
                .active_stream
                .take()
                .expect("active stream was checked above");
            replay_state
                .terminal_stream_message_ids
                .insert(message_id.clone());
            let message = data.message.clone();
            let retains_explicit_stream = !message.tool_calls.is_empty();
            let stream_start = stream.start.clone();
            let stream_text = stream.text.clone();
            let stream_reasoning = stream.reasoning.clone();
            replay_state.recorded_message_senders.insert(
                message
                    .message_id
                    .clone()
                    .expect("validated StreamEnd message id"),
                message.sender.clone(),
            );
            let tool_events = stream.tool_events.clone();
            replay_state.completed_stream = Some(ReplayCompletedStream {
                stream,
                end: StreamEndData {
                    message: message.clone(),
                },
                post_end_events: Vec::new(),
            });
            if retains_explicit_stream {
                push_chat_event_to_replay_log(
                    canonical_stream,
                    event_log,
                    &ChatEvent::StreamStart(stream_start),
                );
                if !stream_reasoning.is_empty() {
                    push_chat_event_to_replay_log(
                        canonical_stream,
                        event_log,
                        &ChatEvent::StreamReasoningDelta(StreamTextDeltaData {
                            message_id: Some(message_id.0.clone()),
                            text: stream_reasoning,
                        }),
                    );
                }
                if !stream_text.is_empty() {
                    push_chat_event_to_replay_log(
                        canonical_stream,
                        event_log,
                        &ChatEvent::StreamDelta(StreamTextDeltaData {
                            message_id: Some(message_id.0.clone()),
                            text: stream_text,
                        }),
                    );
                }
            } else {
                push_chat_event_to_replay_log(
                    canonical_stream,
                    event_log,
                    &ChatEvent::MessageAdded(message.clone()),
                );
            }
            for tool_event in tool_events {
                if let ChatEvent::ToolProgress(data) = &tool_event {
                    coalesce_progress_into_replay_log(
                        canonical_stream,
                        event_log,
                        replay_state,
                        data.tool_call_id.clone(),
                        &tool_event,
                    );
                } else {
                    push_chat_event_to_replay_log(canonical_stream, event_log, &tool_event);
                }
            }
            if retains_explicit_stream {
                push_chat_event_to_replay_log(
                    canonical_stream,
                    event_log,
                    &ChatEvent::StreamEnd(StreamEndData { message }),
                );
            }
        }
        ChatEvent::MessageMetadataUpdated(update) => {
            replay_state.update_completed_stream_metadata(update);
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
                replay_state.update_completed_stream_tool_snapshot(event);
                push_chat_event_to_replay_log(canonical_stream, event_log, event);
            }
        }
        ChatEvent::ToolExecutionCompleted(completion) => {
            if let Some(active) = replay_state.active_stream.as_mut() {
                let belongs_to_active_stream = active.tool_events.iter().any(|buffered| {
                    matches!(
                        buffered,
                        ChatEvent::ToolRequest(request)
                            if request.tool_call_id == completion.tool_call_id
                    )
                });
                if belongs_to_active_stream {
                    active.tool_events.push(event.clone());
                } else {
                    push_chat_event_to_replay_log(canonical_stream, event_log, event);
                }
            } else {
                replay_state.update_completed_stream_tool_snapshot(event);
                push_chat_event_to_replay_log(canonical_stream, event_log, event);
            }
        }
        ChatEvent::ToolProgress(data) => {
            if let Some(active) = replay_state.active_stream.as_mut() {
                let existing = active.tool_events.iter_mut().find(|buffered| {
                    matches!(
                        buffered,
                        ChatEvent::ToolProgress(p) if p.tool_call_id == data.tool_call_id
                    )
                });
                if let Some(existing) = existing {
                    *existing = event.clone();
                } else {
                    active.tool_events.push(event.clone());
                }
            } else {
                replay_state.update_completed_stream_tool_snapshot(event);
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
            if let Some(stream) = replay_state.active_stream.take() {
                replay_state
                    .terminal_stream_message_ids
                    .insert(stream.message_id.clone());
                let message = cancelled_stream_message(&stream);
                if message_has_renderable_content(&message, !stream.tool_events.is_empty()) {
                    replay_state
                        .recorded_message_senders
                        .insert(stream.message_id.clone(), message.sender.clone());
                    push_chat_event_to_replay_log(
                        canonical_stream,
                        event_log,
                        &ChatEvent::MessageAdded(message),
                    );
                }
                for tool_event in stream.tool_events {
                    push_chat_event_to_replay_log(canonical_stream, event_log, &tool_event);
                }
            }
            replay_state.completed_stream = None;
            push_chat_event_to_replay_log(canonical_stream, event_log, event);
        }
        ChatEvent::TypingStatusChanged(typing) => {
            replay_state.typing = *typing;
            if *typing {
                replay_state.resume_history_settled_idle = false;
            }
            if !typing {
                replay_state.completed_stream = None;
            }
            push_chat_event_to_replay_log(canonical_stream, event_log, event);
        }
        ChatEvent::MessageAdded(message) => {
            if let Some(message_id) = &message.message_id {
                replay_state
                    .recorded_message_senders
                    .insert(message_id.clone(), message.sender.clone());
            }
            push_chat_event_to_replay_log(canonical_stream, event_log, event);
        }
        ChatEvent::TaskUpdate(_) | ChatEvent::RetryAttempt(_) | ChatEvent::Orchestration(_) => {
            push_chat_event_to_replay_log(canonical_stream, event_log, event);
        }
    }
    Ok(())
}

fn validate_chat_event_stream_identity(
    replay_state: &AgentReplayState,
    event: &ChatEvent,
) -> Result<(), StreamIdentityViolation> {
    match event {
        ChatEvent::StreamStart(start) => {
            let message_id = required_replay_stream_message_id(start.required_message_id())?;
            if replay_state.active_stream.is_some() {
                return Err(StreamIdentityViolation::ForeignActiveMessageId);
            }
            if replay_state
                .terminal_stream_message_ids
                .contains(&message_id)
            {
                return Err(StreamIdentityViolation::DuplicateTerminalMessageId);
            }
            if replay_state
                .recorded_message_senders
                .contains_key(&message_id)
            {
                return Err(StreamIdentityViolation::DuplicateTerminalMessageId);
            }
        }
        ChatEvent::StreamDelta(delta) | ChatEvent::StreamReasoningDelta(delta) => {
            let message_id = required_replay_stream_message_id(delta.required_message_id())?;
            let Some(active) = replay_state.active_stream.as_ref() else {
                return Err(StreamIdentityViolation::ForeignActiveMessageId);
            };
            if message_id != active.message_id {
                return Err(StreamIdentityViolation::ForeignActiveMessageId);
            }
        }
        ChatEvent::StreamEnd(data) => {
            let message_id = required_replay_chat_message_id(data.required_message_id())?;
            let Some(active) = replay_state.active_stream.as_ref() else {
                return Err(
                    if replay_state
                        .terminal_stream_message_ids
                        .contains(&message_id)
                    {
                        StreamIdentityViolation::ConflictingDuplicateCompletion
                    } else {
                        StreamIdentityViolation::ForeignActiveMessageId
                    },
                );
            };
            if message_id != active.message_id {
                return Err(StreamIdentityViolation::MismatchedEndMessageId);
            }
            if replay_state
                .recorded_message_senders
                .contains_key(&message_id)
            {
                return Err(StreamIdentityViolation::DuplicateTerminalMessageId);
            }
        }
        ChatEvent::MessageAdded(message) => {
            if let Some(message_id) = &message.message_id
                && replay_state
                    .recorded_message_senders
                    .contains_key(message_id)
            {
                return Err(StreamIdentityViolation::DuplicateTerminalMessageId);
            }
        }
        _ => {}
    }
    Ok(())
}

/// How the ingest loop should proceed after a stream identity violation.
enum StreamIdentityRecovery {
    /// The event has been rewritten (or the abandoned stream can be closed)
    /// so processing may continue. `finalize_abandoned` closes the still-open
    /// stream the backend walked away from before the event is applied.
    Resync {
        finalize_abandoned: Option<Box<ChatEvent>>,
    },
    /// No unambiguous interpretation exists; drop the event after reporting.
    Unrecoverable,
}

/// A violation must cost one diagnostic, not the rest of the session. The two
/// resyncable shapes are the ones with exactly one faithful interpretation:
/// an id-less `StreamEnd` while a single stream is active can only be ending
/// that stream, and a fresh `StreamStart` while another stream is active can
/// only mean the backend abandoned the previous stream without ending it.
/// Everything else (duplicates, mismatched ends, orphan deltas) stays
/// report-and-drop because guessing would fabricate transcript state.
fn recover_stream_identity_violation(
    replay_state: &AgentReplayState,
    event: &mut ChatEvent,
    violation: StreamIdentityViolation,
) -> StreamIdentityRecovery {
    match (violation, event) {
        (StreamIdentityViolation::MissingMessageId, ChatEvent::StreamEnd(end)) => {
            let Some(active) = replay_state.active_stream.as_ref() else {
                return StreamIdentityRecovery::Unrecoverable;
            };
            end.message.message_id = Some(active.message_id.clone());
            StreamIdentityRecovery::Resync {
                finalize_abandoned: None,
            }
        }
        (StreamIdentityViolation::ForeignActiveMessageId, ChatEvent::StreamStart(_)) => {
            let Some(active) = replay_state.active_stream.as_ref() else {
                return StreamIdentityRecovery::Unrecoverable;
            };
            StreamIdentityRecovery::Resync {
                finalize_abandoned: Some(Box::new(synthesized_end_for_abandoned_stream(active))),
            }
        }
        _ => StreamIdentityRecovery::Unrecoverable,
    }
}

fn synthesized_end_for_abandoned_stream(active: &ReplayActiveStream) -> ChatEvent {
    ChatEvent::StreamEnd(StreamEndData {
        message: cancelled_stream_message(active),
    })
}

fn required_replay_stream_message_id(
    message_id: Result<ChatMessageId, StreamIdentityViolation>,
) -> Result<ChatMessageId, StreamIdentityViolation> {
    message_id
}

fn required_replay_chat_message_id(
    message_id: Result<ChatMessageId, StreamIdentityViolation>,
) -> Result<ChatMessageId, StreamIdentityViolation> {
    message_id
}

fn stream_identity_violation_event(violation: StreamIdentityViolation) -> ChatEvent {
    let content = match violation {
        StreamIdentityViolation::MissingMessageId => {
            "Stream identity violation: missing message id"
        }
        StreamIdentityViolation::ForeignActiveMessageId => {
            "Stream identity violation: foreign active message id"
        }
        StreamIdentityViolation::MismatchedEndMessageId => {
            "Stream identity violation: mismatched end message id"
        }
        StreamIdentityViolation::DuplicateTerminalMessageId => {
            "Stream identity violation: duplicate terminal message id"
        }
        StreamIdentityViolation::ConflictingDuplicateCompletion => {
            "Stream identity violation: conflicting duplicate completion"
        }
    };
    ChatEvent::MessageAdded(ChatMessage {
        message_id: None,
        timestamp: now_ms(),
        sender: MessageSender::Error,
        content: content.to_owned(),
        reasoning: None,
        tool_calls: Vec::new(),
        model_info: None,
        token_usage: None,
        context_breakdown: None,
        images: None,
    })
}

fn cancelled_stream_message(stream: &ReplayActiveStream) -> ChatMessage {
    ChatMessage {
        message_id: Some(stream.message_id.clone()),
        timestamp: now_ms(),
        sender: MessageSender::Assistant {
            agent: stream.start.agent.clone(),
        },
        content: stream.text.clone(),
        reasoning: (!stream.reasoning.is_empty()).then(|| ReasoningData {
            text: stream.reasoning.clone(),
            tokens: None,
            signature: None,
            blob: None,
        }),
        tool_calls: Vec::new(),
        model_info: stream.start.model.clone().map(|model| ModelInfo { model }),
        token_usage: None,
        context_breakdown: None,
        images: None,
    }
}

fn message_has_renderable_content(message: &ChatMessage, has_tool_events: bool) -> bool {
    !message.content.trim().is_empty()
        || message
            .reasoning
            .as_ref()
            .is_some_and(|reasoning| !reasoning.text.trim().is_empty())
        || !message.tool_calls.is_empty()
        || message
            .images
            .as_ref()
            .is_some_and(|images| !images.is_empty())
        || has_tool_events
}

fn same_chat_message(expected: &ChatMessage, actual: &ChatMessage) -> bool {
    if let Some(message_id) = &expected.message_id {
        return actual.message_id.as_ref() == Some(message_id);
    }
    actual.message_id.is_none()
        && actual.timestamp == expected.timestamp
        && actual.content == expected.content
        && same_message_sender(&actual.sender, &expected.sender)
}

fn same_message_sender(left: &MessageSender, right: &MessageSender) -> bool {
    match (left, right) {
        (MessageSender::User, MessageSender::User)
        | (MessageSender::System, MessageSender::System)
        | (MessageSender::Warning, MessageSender::Warning)
        | (MessageSender::Error, MessageSender::Error) => true,
        (MessageSender::Assistant { agent: left }, MessageSender::Assistant { agent: right }) => {
            left == right
        }
        _ => false,
    }
}

fn chat_event_tool_call_id(event: &ChatEvent) -> Option<&str> {
    match event {
        ChatEvent::ToolRequest(request) => Some(request.tool_call_id.as_str()),
        ChatEvent::ToolProgress(progress) => Some(progress.tool_call_id.as_str()),
        ChatEvent::ToolExecutionCompleted(completion) => Some(completion.tool_call_id.as_str()),
        _ => None,
    }
}

fn upsert_tool_event(events: &mut Vec<ChatEvent>, event: &ChatEvent) -> bool {
    let Some(tool_call_id) = chat_event_tool_call_id(event) else {
        return false;
    };
    for existing in events {
        let Some(existing_tool_call_id) = chat_event_tool_call_id(existing) else {
            continue;
        };
        if existing_tool_call_id == tool_call_id && same_tool_event_kind(existing, event) {
            *existing = event.clone();
            return true;
        }
    }
    false
}

fn same_tool_event_kind(left: &ChatEvent, right: &ChatEvent) -> bool {
    matches!(
        (left, right),
        (ChatEvent::ToolRequest(_), ChatEvent::ToolRequest(_))
            | (ChatEvent::ToolProgress(_), ChatEvent::ToolProgress(_))
            | (
                ChatEvent::ToolExecutionCompleted(_),
                ChatEvent::ToolExecutionCompleted(_)
            )
    )
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

#[cfg(test)]
pub(crate) fn task_usage_scope_from_chat_events_for_test(
    backend_kind: BackendKind,
    events: impl IntoIterator<Item = ChatEvent>,
) -> TaskTokenUsageScope {
    let mut tracker = AgentActivityStatsTracker::for_backend(backend_kind);
    for (source_seq, mut event) in events.into_iter().enumerate() {
        tracker.observe_chat_event(&mut event, source_seq as u64, "");
    }
    tracker.usage_snapshot().0
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
        ChatEvent::ToolRequest(request) => Some(format!("Tool requested: {}", request.tool_name)),
        ChatEvent::ToolProgress(progress) => Some(format!("Tool progress: {}", progress.tool_name)),
        ChatEvent::ToolExecutionCompleted(completion) => Some(format!(
            "Tool {} {}",
            completion.tool_name,
            if completion.success {
                "completed"
            } else {
                "failed"
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
    queue: &VecDeque<QueuedMessageEntry>,
) {
    let payload = QueuedMessagesPayload {
        messages: queue.iter().cloned().collect(),
    };
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

#[cfg(test)]
fn attach_subscriber(
    event_log: &[Envelope],
    replay_state: Option<&AgentReplayState>,
    subscribers: &mut Vec<Stream>,
    stream: Stream,
) -> bool {
    let mut latest_output = AgentControlLatestOutput::default();
    latest_output
        .observe_event_log(event_log)
        .expect("typed agent replay log must project latest output");
    attach_subscriber_with_latest_output(
        event_log,
        replay_state,
        latest_output.output(),
        subscribers,
        stream,
    )
}

fn attach_subscriber_with_latest_output(
    event_log: &[Envelope],
    replay_state: Option<&AgentReplayState>,
    latest_output: &AgentControlOutput,
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
    replay_state: Option<&AgentReplayState>,
) -> Vec<(u64, ChatEvent)> {
    let completed_stream_filter =
        replay_state.and_then(AgentReplayState::active_completed_stream_history_filter);
    session_history_entries_from_log(event_log)
        .into_iter()
        .filter(|(_, event)| {
            completed_stream_filter
                .as_ref()
                .is_none_or(|filter| !filter.matches(event))
        })
        .collect()
}

fn initial_history_tail_entries(entries: &[(u64, ChatEvent)]) -> Vec<(u64, ChatEvent)> {
    let start = history_start_for_message_limit(entries, entries.len(), INITIAL_HISTORY_TAIL_LIMIT);
    entries[start..].to_vec()
}

fn agent_bootstrap_events_from_log(event_log: &[Envelope]) -> Vec<AgentBootstrapEvent> {
    let mut events = Vec::new();
    for envelope in event_log {
        if matches!(
            envelope.kind,
            FrameKind::AgentStart
                | FrameKind::AgentError
                | FrameKind::SessionSettings
                | FrameKind::QueuedMessages
                | FrameKind::AgentActivityStats
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
    let ChatEvent::StreamEnd(end) = &entries[terminal_index].1 else {
        return terminal_index;
    };
    let Some(message_id) = end.message.message_id.as_ref() else {
        return terminal_index;
    };
    entries[..terminal_index]
        .iter()
        .rposition(|(_, event)| {
            matches!(
                event,
                ChatEvent::StreamStart(start)
                    if start.message_id.as_deref() == Some(message_id.0.as_str())
            )
        })
        .unwrap_or(terminal_index)
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
                    .unwrap_or(request.tool_name.as_str());
                if prompt.is_some()
                    && matches!(action.to_ascii_lowercase().as_str(), "spawn" | "spawnagent")
                {
                    request.tool_type = ToolRequestType::AgentSpawn { prompt, name };
                } else {
                    request.tool_type = ToolRequestType::Other {
                        args: serde_json::json!({
                            "action": action,
                            "agent_count": legacy_codex_agent_count(args),
                        }),
                    };
                }
            } else if legacy_claude_agent_request(&request.tool_name, args) {
                let prompt = ["prompt", "task", "instruction", "message"]
                    .into_iter()
                    .find_map(|key| nonempty_json_string(args, key));
                let name = nonempty_json_string(args, "description")
                    .or_else(|| nonempty_json_string(args, "subagent_type"));
                request.tool_type = ToolRequestType::AgentSpawn { prompt, name };
            }
        }
        ChatEvent::ToolExecutionCompleted(completion) => {
            let ToolExecutionResult::Other { result } = &completion.tool_result else {
                return;
            };
            if legacy_codex_collaboration_value(result) {
                let action = result
                    .get("tool")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or(completion.tool_name.as_str());
                completion.tool_result = ToolExecutionResult::Other {
                    result: serde_json::json!({
                        "action": action,
                        "status": if completion.success { "completed" } else { "failed" },
                        "agent_count": legacy_codex_agent_count(result),
                    }),
                };
                completion.error =
                    (!completion.success).then(|| format!("{} failed", completion.tool_name));
            } else if legacy_claude_agent_result(&completion.tool_name, result) {
                completion.tool_result = ToolExecutionResult::Other {
                    result: serde_json::json!({
                        "status": if completion.success { "completed" } else { "failed" },
                    }),
                };
                completion.error =
                    (!completion.success).then(|| format!("{} failed", completion.tool_name));
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

fn legacy_claude_agent_request(tool_name: &str, args: &serde_json::Value) -> bool {
    matches!(tool_name, "Task" | "Agent")
        && args.get("prompt").is_some()
        && [
            "subagent_type",
            "run_in_background",
            "description",
            "resume",
        ]
        .into_iter()
        .any(|key| args.get(key).is_some())
}

fn legacy_claude_agent_result(tool_name: &str, result: &serde_json::Value) -> bool {
    matches!(tool_name, "Task" | "Agent")
        && [
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
    let mut events = Vec::new();
    for envelope in event_log {
        if envelope.kind != FrameKind::ChatEvent {
            continue;
        }
        let mut event: ChatEvent = serde_json::from_value(envelope.payload.clone())
            .expect("failed to parse ChatEvent from replay log");
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
            event => events.push((envelope.seq, event)),
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
) {
    let result = {
        let store = session_store.lock().await;
        match event {
            ChatEvent::StreamEnd(data) => store.update(session_id, |record| {
                record.updated_at_ms = now_ms();
                record.message_count += 1;
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
    }
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;

    use protocol::{
        AgentActivityStats, AgentActivityStatsPayload, AgentBootstrapEvent, AgentBootstrapPayload,
        AgentControlLatestOutput, AgentControlOutput, AgentControlStatus, AgentId, AgentInput,
        AgentStartPayload, BackendKind, ChatEvent, ChatMessage, ChatMessageId, FrameKind,
        MessageMetadataUpdateData, MessageSender, MessageTokenUsage, ModelInfo, ModelRequestId,
        ModelRequestTokenUsage, ModelTurnId, ReasoningData, ServerGeneratedChatMessageIdOrigin,
        ServerGeneratedChatMessageIdentity, SessionId, StreamEndData, StreamPath, StreamStartData,
        StreamTextDeltaData, TaskList, TaskTokenUsageScope, TaskTokenUsageUnavailableReason,
        TokenUsage, TokenUsageScope, TokenUsageUnavailableReason, ToolExecutionCompletedData,
        ToolExecutionResult, ToolRequest, ToolRequestType, ToolUseData,
    };
    use tokio::sync::{Mutex, mpsc, watch};
    use tokio::time::timeout;

    use super::{
        AGENT_STARTUP_SELECTION_TEST_GATE, AGENT_STARTUP_TEST_GATE, AgentActivityStatsTracker,
        AgentActorRuntimeContext, AgentCommand, AgentHandle, AgentNameChangeContext,
        AgentReplayState, AgentStartupFailure, AgentStartupTestGate,
        GenerateAgentActivitySummaryRequest, InterruptOutcome, RelayEventReceivers,
        ResolvedSpawnRequest, activity_history_snapshot, agent_name_generation_spawn_config,
        agent_usage_snapshot_from_log, append_chat_event, append_event, apply_generated_agent_name,
        attach_subscriber, attach_subscriber_with_latest_output,
        collect_agent_activity_summary_events, collect_agent_name_events, current_latest_output,
        generate_mock_name, ingest_gated_replay_event, known_turn_usage, mark_agent_turn_active,
        output_events_since, project_legacy_native_collaboration_event, publish_resumed_agent_idle,
        record_agent_started, replay_envelope, resolve_backend_session_settings,
        sanitize_generated_agent_name, session_history_entries_from_log, session_history_window,
        spawn_agent_actor, spawn_relay_agent_actor, terminal_input_rejected_payload,
        upsert_activity_stats_snapshot,
    };
    use crate::agent::customization::ResolvedSpawnConfig;
    use crate::agent::registry::AgentStatusHandle;
    use crate::backend::{BackendExecutionMode, BackendSpawnConfig, EventStream};
    use crate::review::ReviewRegistry;
    use crate::store::project::ProjectStore;
    use crate::store::review::ReviewStore;
    use crate::store::session::SessionStore;
    use crate::stream::Stream;

    static AGENT_STARTUP_ACTOR_TEST_LOCK: Mutex<()> = Mutex::const_new(());

    fn recovery_stream_start(message_id: &str) -> ChatEvent {
        ChatEvent::StreamStart(StreamStartData {
            message_id: Some(message_id.to_owned()),
            agent: "tycode".to_owned(),
            model: Some("qwen-plus".to_owned()),
        })
    }

    fn recovery_stream_delta(message_id: &str, text: &str) -> ChatEvent {
        ChatEvent::StreamDelta(StreamTextDeltaData {
            message_id: Some(message_id.to_owned()),
            text: text.to_owned(),
        })
    }

    fn recovery_id_less_stream_end(content: &str) -> ChatEvent {
        ChatEvent::StreamEnd(StreamEndData {
            message: ChatMessage {
                message_id: None,
                timestamp: 1,
                sender: MessageSender::Assistant {
                    agent: "tycode".to_owned(),
                },
                content: content.to_owned(),
                reasoning: None,
                tool_calls: Vec::new(),
                model_info: None,
                token_usage: None,
                context_breakdown: None,
                images: None,
            },
        })
    }

    fn record_recovery_event(
        event_log: &mut Vec<super::Envelope>,
        replay_state: &mut AgentReplayState,
        event: &ChatEvent,
    ) {
        super::record_chat_event_for_replay("/agent/recovery-test", event_log, replay_state, event)
            .expect("recovery test event must validate");
    }

    /// An id-less `StreamEnd` while its stream is active is the tycode 0.10.0
    /// wire shape; recovery must adopt the active id so the completion (and
    /// its metadata) lands instead of poisoning every later turn.
    #[test]
    fn stream_identity_missing_end_id_recovers_without_poisoning_session() {
        let mut event_log = Vec::new();
        let mut replay_state = AgentReplayState::default();
        record_recovery_event(
            &mut event_log,
            &mut replay_state,
            &recovery_stream_start("m1"),
        );
        record_recovery_event(
            &mut event_log,
            &mut replay_state,
            &recovery_stream_delta("m1", "pong"),
        );

        let mut end = recovery_id_less_stream_end("pong");
        let violation = super::validate_chat_event_stream_identity(&replay_state, &end)
            .expect_err("id-less StreamEnd must be flagged");
        assert_eq!(
            violation,
            protocol::StreamIdentityViolation::MissingMessageId
        );

        let recovery = super::recover_stream_identity_violation(&replay_state, &mut end, violation);
        assert!(matches!(
            recovery,
            super::StreamIdentityRecovery::Resync {
                finalize_abandoned: None
            }
        ));
        let ChatEvent::StreamEnd(recovered) = &end else {
            unreachable!()
        };
        assert_eq!(
            recovered.message.message_id,
            Some(ChatMessageId("m1".to_owned()))
        );
        record_recovery_event(&mut event_log, &mut replay_state, &end);
        assert!(replay_state.active_stream.is_none());

        // The next turn must open cleanly: no cascade.
        super::validate_chat_event_stream_identity(&replay_state, &recovery_stream_start("m2"))
            .expect("next turn must not inherit a stuck stream");
    }

    /// A fresh `StreamStart` while another stream is active means the backend
    /// abandoned the previous stream; recovery finalizes the abandoned stream
    /// with its accumulated content before accepting the new one.
    #[test]
    fn stream_identity_foreign_start_recovery_finalizes_abandoned_stream() {
        let mut event_log = Vec::new();
        let mut replay_state = AgentReplayState::default();
        record_recovery_event(
            &mut event_log,
            &mut replay_state,
            &recovery_stream_start("m1"),
        );
        record_recovery_event(
            &mut event_log,
            &mut replay_state,
            &recovery_stream_delta("m1", "partial answer"),
        );

        let mut start = recovery_stream_start("m2");
        let violation = super::validate_chat_event_stream_identity(&replay_state, &start)
            .expect_err("start over an active stream must be flagged");
        assert_eq!(
            violation,
            protocol::StreamIdentityViolation::ForeignActiveMessageId
        );

        let recovery =
            super::recover_stream_identity_violation(&replay_state, &mut start, violation);
        let super::StreamIdentityRecovery::Resync {
            finalize_abandoned: Some(finalize),
        } = recovery
        else {
            panic!("foreign start over an active stream must resync with a finalize");
        };
        let ChatEvent::StreamEnd(finalize_end) = &*finalize else {
            panic!("finalize event must be a StreamEnd");
        };
        assert_eq!(
            finalize_end.message.message_id,
            Some(ChatMessageId("m1".to_owned()))
        );
        assert_eq!(finalize_end.message.content, "partial answer");

        record_recovery_event(&mut event_log, &mut replay_state, &finalize);
        record_recovery_event(&mut event_log, &mut replay_state, &start);
        assert_eq!(
            replay_state
                .active_stream
                .as_ref()
                .map(|active| active.message_id.0.as_str()),
            Some("m2")
        );
        assert!(
            replay_state
                .terminal_stream_message_ids
                .contains(&ChatMessageId("m1".to_owned()))
        );
    }

    /// Shapes without one faithful interpretation stay report-and-drop:
    /// recovery must not fabricate transcript state.
    #[test]
    fn stream_identity_ambiguous_shapes_stay_unrecoverable() {
        let mut event_log = Vec::new();
        let mut replay_state = AgentReplayState::default();

        // An id-less end with no active stream has nothing to adopt.
        let mut orphan_end = recovery_id_less_stream_end("orphan");
        let violation =
            super::validate_chat_event_stream_identity(&replay_state, &orphan_end).unwrap_err();
        assert!(matches!(
            super::recover_stream_identity_violation(&replay_state, &mut orphan_end, violation),
            super::StreamIdentityRecovery::Unrecoverable
        ));

        // A delta for a foreign id must not be grafted onto the active stream.
        record_recovery_event(
            &mut event_log,
            &mut replay_state,
            &recovery_stream_start("m1"),
        );
        let mut foreign_delta = recovery_stream_delta("m9", "stray");
        let violation =
            super::validate_chat_event_stream_identity(&replay_state, &foreign_delta).unwrap_err();
        assert!(matches!(
            super::recover_stream_identity_violation(&replay_state, &mut foreign_delta, violation),
            super::StreamIdentityRecovery::Unrecoverable
        ));
    }

    fn startup_actor_fixture(
        agent_id: &str,
        startup_failure: Option<AgentStartupFailure>,
    ) -> (
        tempfile::TempDir,
        AgentStartPayload,
        ResolvedSpawnRequest,
        AgentActorRuntimeContext,
        AgentStatusHandle,
    ) {
        let dir = tempfile::tempdir().expect("agent startup tempdir");
        let session_store = Arc::new(Mutex::new(
            SessionStore::load(dir.path().join("sessions.json"))
                .expect("load startup session store"),
        ));
        let project_store = Arc::new(Mutex::new(
            ProjectStore::load(dir.path().join("projects.json"))
                .expect("load startup project store"),
        ));
        let review_store =
            ReviewStore::load(dir.path().join("reviews.json")).expect("load review store");
        let (review_delivery_tx, _review_delivery_rx) = mpsc::channel(1);
        let (review_ai_spawn_tx, _review_ai_spawn_rx) = mpsc::channel(1);
        let (review_project_update_tx, _review_project_update_rx) = mpsc::unbounded_channel();
        let review_registry = ReviewRegistry::spawn(
            review_store,
            project_store,
            review_delivery_tx,
            review_ai_spawn_tx,
            review_project_update_tx,
        )
        .expect("spawn review registry");
        let (host_sub_agent_spawn_tx, _host_sub_agent_spawn_rx) = mpsc::unbounded_channel();
        let (capacity_tx, _capacity_rx) = mpsc::unbounded_channel();
        let (status_handle, _status_rx) = AgentStatusHandle::new();
        let start = AgentStartPayload {
            backend_kind: protocol::BackendKind::Claude,
            ..test_agent_start(agent_id)
        };
        let request = ResolvedSpawnRequest {
            name: start.name.clone(),
            origin: start.origin,
            custom_agent_id: None,
            team_id: None,
            team_member_id: None,
            workflow: None,
            parent_agent_id: None,
            parent_session_id: None,
            project_id: None,
            backend_kind: protocol::BackendKind::Claude,
            launch_profile_id: None,
            workspace_roots: start.workspace_roots.clone(),
            initial_input: Some(protocol::SendMessagePayload {
                message: "startup attachment ordering".to_owned(),
                images: None,
                origin: None,
                tool_response: None,
            }),
            cost_hint: None,
            session_settings: None,
            session_settings_schema: None,
            backend_config: Default::default(),
            startup_mcp_servers: Vec::new(),
            resolved_spawn_config: ResolvedSpawnConfig::default(),
            resume_session_id: None,
            fork_from_session_id: None,
            startup_warning: None,
            startup_failure,
            initial_alias: None,
            use_mock_backend: true,
        };
        let runtime = AgentActorRuntimeContext {
            session_store,
            host_sub_agent_spawn_tx,
            capacity_tx,
            review_registry,
            status_handle: status_handle.clone(),
            antigravity_conversations_dir: dir.path().join("antigravity"),
        };
        (dir, start, request, runtime, status_handle)
    }

    fn install_agent_startup_gate(
        agent_id: AgentId,
    ) -> (
        tokio::sync::oneshot::Receiver<()>,
        tokio::sync::oneshot::Sender<()>,
    ) {
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        *AGENT_STARTUP_TEST_GATE
            .lock()
            .expect("agent startup test gate mutex poisoned") = Some(AgentStartupTestGate {
            agent_id,
            entered: entered_tx,
            release: release_rx,
        });
        (entered_rx, release_tx)
    }

    #[tokio::test]
    async fn actor_interrupt_parks_terminal_while_close_ends_startup() {
        let _startup_test_guard = AGENT_STARTUP_ACTOR_TEST_LOCK.lock().await;
        for (close, simultaneous_ready) in [(false, false), (true, false), (false, true)] {
            let dir = tempfile::tempdir().expect("agent startup tempdir");
            let session_store = Arc::new(Mutex::new(
                SessionStore::load(dir.path().join("sessions.json"))
                    .expect("load startup session store"),
            ));
            let project_store = Arc::new(Mutex::new(
                ProjectStore::load(dir.path().join("projects.json"))
                    .expect("load startup project store"),
            ));
            let review_store =
                ReviewStore::load(dir.path().join("reviews.json")).expect("load review store");
            let (review_delivery_tx, _review_delivery_rx) = mpsc::channel(1);
            let (review_ai_spawn_tx, _review_ai_spawn_rx) = mpsc::channel(1);
            let (review_project_update_tx, _review_project_update_rx) = mpsc::unbounded_channel();
            let review_registry = ReviewRegistry::spawn(
                review_store,
                project_store,
                review_delivery_tx,
                review_ai_spawn_tx,
                review_project_update_tx,
            )
            .expect("spawn review registry");
            let (host_sub_agent_spawn_tx, _host_sub_agent_spawn_rx) = mpsc::unbounded_channel();
            let (capacity_tx, _capacity_rx) = mpsc::unbounded_channel();
            let (status_handle, _status_rx) = AgentStatusHandle::new();
            let start = AgentStartPayload {
                backend_kind: protocol::BackendKind::Claude,
                ..test_agent_start(if simultaneous_ready {
                    "startup-simultaneous-interrupt-agent"
                } else if close {
                    "startup-close-agent"
                } else {
                    "startup-interrupt-agent"
                })
            };
            let request = ResolvedSpawnRequest {
                name: start.name.clone(),
                origin: start.origin,
                custom_agent_id: None,
                team_id: None,
                team_member_id: None,
                workflow: None,
                parent_agent_id: None,
                parent_session_id: None,
                project_id: None,
                backend_kind: protocol::BackendKind::Claude,
                launch_profile_id: None,
                workspace_roots: start.workspace_roots.clone(),
                initial_input: Some(protocol::SendMessagePayload {
                    message: "must never publish".to_owned(),
                    images: None,
                    origin: None,
                    tool_response: None,
                }),
                cost_hint: None,
                session_settings: None,
                session_settings_schema: None,
                backend_config: Default::default(),
                startup_mcp_servers: Vec::new(),
                resolved_spawn_config: ResolvedSpawnConfig::default(),
                resume_session_id: None,
                fork_from_session_id: None,
                startup_warning: None,
                startup_failure: None,
                initial_alias: None,
                use_mock_backend: true,
            };
            let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
            let (release_tx, release_rx) = tokio::sync::oneshot::channel();
            let mut release_tx = Some(release_tx);
            let gate = AgentStartupTestGate {
                agent_id: start.agent_id.clone(),
                entered: entered_tx,
                release: release_rx,
            };
            let gate_slot = if simultaneous_ready {
                &AGENT_STARTUP_SELECTION_TEST_GATE
            } else {
                &AGENT_STARTUP_TEST_GATE
            };
            *gate_slot
                .lock()
                .expect("agent startup test gate mutex poisoned") = Some(gate);
            let (handle, startup) = spawn_agent_actor(
                start.agent_id.clone(),
                start,
                request,
                AgentActorRuntimeContext {
                    session_store: Arc::clone(&session_store),
                    host_sub_agent_spawn_tx,
                    capacity_tx,
                    review_registry,
                    status_handle: status_handle.clone(),
                    antigravity_conversations_dir: dir.path().join("antigravity"),
                },
            );
            timeout(Duration::from_secs(1), entered_rx)
                .await
                .expect("actor must enter delayed backend startup")
                .expect("actor must signal delayed backend startup");
            let (output_tx, mut output_rx) = mpsc::unbounded_channel();
            let (attach_reply, attach_response) = tokio::sync::oneshot::channel();
            handle
                .tx
                .send(AgentCommand::Attach {
                    stream: replay_stream(output_tx),
                    reply: attach_reply,
                })
                .expect("queue startup attach before cancellation");
            if simultaneous_ready {
                let (interrupt_reply, interrupt_response) = tokio::sync::oneshot::channel();
                handle
                    .tx
                    .send(AgentCommand::Interrupt {
                        reply: interrupt_reply,
                    })
                    .expect("queue simultaneous startup interrupt");
                release_tx
                    .take()
                    .expect("simultaneous startup release sender")
                    .send(())
                    .expect("release simultaneous startup selection");
                assert!(
                    timeout(Duration::from_secs(1), attach_response)
                        .await
                        .expect("simultaneous startup attach must be bounded")
                        .expect("simultaneous startup attach reply")
                );
                assert_eq!(
                    timeout(Duration::from_secs(1), interrupt_response)
                        .await
                        .expect("simultaneous startup interrupt must be bounded")
                        .expect("simultaneous startup interrupt reply"),
                    InterruptOutcome::Interrupted
                );
            } else if close {
                assert!(
                    timeout(Duration::from_secs(1), handle.close())
                        .await
                        .expect("close during startup must be bounded")
                );
                assert!(
                    timeout(Duration::from_secs(1), attach_response)
                        .await
                        .expect("startup attach before close must be bounded")
                        .expect("startup attach before close reply")
                );
            } else {
                assert_eq!(
                    timeout(Duration::from_secs(1), handle.interrupt())
                        .await
                        .expect("interrupt during startup must be bounded"),
                    InterruptOutcome::Interrupted
                );
                assert!(
                    timeout(Duration::from_secs(1), attach_response)
                        .await
                        .expect("startup attach before interrupt must be bounded")
                        .expect("startup attach before interrupt reply")
                );
            }
            if !simultaneous_ready {
                assert!(
                    release_tx
                        .take()
                        .expect("delayed startup release sender")
                        .send(())
                        .is_err(),
                    "startup future must be dropped"
                );
            }
            let startup_error = timeout(Duration::from_secs(1), startup)
                .await
                .expect("startup cancellation response must be bounded")
                .expect("actor must send its typed startup cancellation result")
                .expect_err("startup must report cancellation");
            assert_eq!(
                startup_error,
                if close {
                    "agent startup closed"
                } else {
                    "agent startup interrupted"
                }
            );
            let status = status_handle.snapshot().await;
            assert!(status.terminated);
            assert!(!status.is_thinking);
            assert!(status.turn_completed);
            assert_eq!(
                status.status(),
                if close {
                    protocol::AgentControlStatus::Idle
                } else {
                    protocol::AgentControlStatus::Failed
                }
            );
            if close {
                assert!(
                    timeout(Duration::from_secs(1), output_rx.recv())
                        .await
                        .expect("closed startup subscriber must close")
                        .is_none(),
                    "startup close is completed by the authoritative AgentClosed lifecycle"
                );
            } else {
                let first = timeout(Duration::from_secs(1), output_rx.recv())
                    .await
                    .expect("interrupted startup must bootstrap terminal state")
                    .expect("interrupted startup subscriber remains attached");
                assert_eq!(first.kind, FrameKind::AgentBootstrap);
                let bootstrap: AgentBootstrapPayload = first
                    .parse_payload()
                    .expect("interrupted startup AgentBootstrap");
                assert!(bootstrap.events.iter().any(|event| matches!(
                    event,
                    AgentBootstrapEvent::AgentError(error)
                        if error.fatal && error.message.contains("agent startup interrupted")
                )));
                assert!(handle.close().await);
            }
            assert!(
                session_store
                    .lock()
                    .await
                    .list()
                    .expect("list startup sessions")
                    .is_empty(),
                "startup cancellation must not persist a session"
            );
        }
    }

    #[tokio::test]
    async fn pending_startup_attachments_receive_complete_bootstrap_before_live_events() {
        let _startup_test_guard = AGENT_STARTUP_ACTOR_TEST_LOCK.lock().await;
        let (_dir, start, request, runtime, _status_handle) =
            startup_actor_fixture("startup-attach-success", None);
        let expected_settings = resolve_backend_session_settings(
            protocol::BackendKind::Claude,
            &BackendSpawnConfig {
                execution_mode: BackendExecutionMode::Agent,
                resolved_spawn_config: ResolvedSpawnConfig::default(),
                ..BackendSpawnConfig::default()
            },
        );
        let (entered_rx, release_tx) = install_agent_startup_gate(start.agent_id.clone());
        let (handle, startup_rx) =
            spawn_agent_actor(start.agent_id.clone(), start, request, runtime);
        entered_rx
            .await
            .expect("actor must enter delayed backend startup");

        let (output_tx, mut output_rx) = mpsc::unbounded_channel();
        let (attach_reply, attach_rx) = tokio::sync::oneshot::channel();
        handle
            .tx
            .send(AgentCommand::Attach {
                stream: replay_stream(output_tx),
                reply: attach_reply,
            })
            .expect("queue pending startup attachment");
        let (dead_output_tx, dead_output_rx) = mpsc::unbounded_channel();
        drop(dead_output_rx);
        let (dead_attach_reply, dead_attach_rx) = tokio::sync::oneshot::channel();
        handle
            .tx
            .send(AgentCommand::Attach {
                stream: replay_stream(dead_output_tx),
                reply: dead_attach_reply,
            })
            .expect("queue dead pending startup attachment");

        release_tx
            .send(())
            .expect("release delayed backend startup");
        startup_rx
            .await
            .expect("actor must report startup result")
            .expect("mock backend startup must succeed");
        assert!(attach_rx.await.expect("pending attachment reply"));
        assert!(!dead_attach_rx.await.expect("dead pending attachment reply"));

        let first = output_rx
            .recv()
            .await
            .expect("pending attachment must receive bootstrap");
        assert_eq!(first.kind, FrameKind::AgentBootstrap);
        let bootstrap: AgentBootstrapPayload = first
            .parse_payload()
            .expect("startup AgentBootstrap payload");
        assert_eq!(bootstrap.events.len(), 4);
        let expected_start = handle.snapshot();
        let AgentBootstrapEvent::AgentStart(bootstrap_start) = &bootstrap.events[0] else {
            panic!("startup bootstrap must begin with AgentStart");
        };
        assert_eq!(
            serde_json::to_value(bootstrap_start).expect("serialize bootstrap AgentStart"),
            serde_json::to_value(&expected_start).expect("serialize expected AgentStart")
        );
        assert!(matches!(
            &bootstrap.events[1],
            AgentBootstrapEvent::AgentActivityStats(payload)
                if payload.agent_id == expected_start.agent_id
                    && payload.stats == AgentActivityStats::default()
        ));
        assert!(matches!(
            &bootstrap.events[2],
            AgentBootstrapEvent::SessionSettings(payload) if payload.values == expected_settings
        ));
        assert!(matches!(
            &bootstrap.events[3],
            AgentBootstrapEvent::QueuedMessages(payload) if payload.messages.is_empty()
        ));

        assert!(handle.close().await);
        while let Some(envelope) = output_rx.recv().await {
            assert_ne!(
                envelope.kind,
                FrameKind::AgentStart,
                "startup AgentStart must remain inside AgentBootstrap"
            );
        }
    }

    #[tokio::test]
    async fn pending_startup_attachment_failure_receives_terminal_bootstrap() {
        let _startup_test_guard = AGENT_STARTUP_ACTOR_TEST_LOCK.lock().await;
        let failure_message = "fixture startup failure";
        let (_dir, start, request, runtime, _status_handle) = startup_actor_fixture(
            "startup-attach-failure",
            Some(AgentStartupFailure::backend_failed(failure_message)),
        );
        let (entered_rx, release_tx) = install_agent_startup_gate(start.agent_id.clone());
        let (handle, startup_rx) =
            spawn_agent_actor(start.agent_id.clone(), start.clone(), request, runtime);
        entered_rx
            .await
            .expect("actor must enter delayed failing startup");

        let (output_tx, mut output_rx) = mpsc::unbounded_channel();
        let (attach_reply, attach_rx) = tokio::sync::oneshot::channel();
        handle
            .tx
            .send(AgentCommand::Attach {
                stream: replay_stream(output_tx),
                reply: attach_reply,
            })
            .expect("queue pending failing-startup attachment");
        release_tx
            .send(())
            .expect("release delayed failing startup");

        let startup_error = startup_rx
            .await
            .expect("actor must report startup failure")
            .expect_err("fixture startup must fail");
        assert_eq!(startup_error, failure_message);
        assert!(attach_rx.await.expect("failing startup attachment reply"));
        let first = output_rx
            .recv()
            .await
            .expect("failing startup attachment must receive terminal bootstrap");
        assert_eq!(first.kind, FrameKind::AgentBootstrap);
        let bootstrap: AgentBootstrapPayload = first
            .parse_payload()
            .expect("terminal startup AgentBootstrap payload");
        let AgentBootstrapEvent::AgentStart(bootstrap_start) = &bootstrap.events[0] else {
            panic!("terminal bootstrap must begin with AgentStart");
        };
        assert_eq!(
            serde_json::to_value(bootstrap_start).expect("serialize terminal AgentStart"),
            serde_json::to_value(&start).expect("serialize expected terminal AgentStart")
        );
        assert!(bootstrap.events.iter().any(|event| matches!(
            event,
            AgentBootstrapEvent::AgentActivityStats(payload)
                if payload.agent_id == start.agent_id
                    && payload.stats == AgentActivityStats::default()
        )));
        assert!(bootstrap.events.iter().any(|event| matches!(
            event,
            AgentBootstrapEvent::QueuedMessages(payload) if payload.messages.is_empty()
        )));
        assert!(bootstrap.events.iter().any(|event| matches!(
            event,
            AgentBootstrapEvent::AgentError(payload)
                if payload.fatal && payload.message.contains(failure_message)
        )));

        assert!(handle.close().await);
        while let Some(envelope) = output_rx.recv().await {
            assert_ne!(
                envelope.kind,
                FrameKind::AgentStart,
                "failed startup AgentStart must remain inside terminal AgentBootstrap"
            );
        }
    }

    fn spawn_failed_agent_actor(
        start: AgentStartPayload,
        error: String,
        status_handle: AgentStatusHandle,
    ) -> AgentHandle {
        let (tx, mut rx) = mpsc::unbounded_channel::<AgentCommand>();
        let accepting_input = Arc::new(AtomicBool::new(false));
        let accepting_input_task = Arc::clone(&accepting_input);
        let closing = Arc::new(AtomicBool::new(false));
        let (_start_tx, start_rx) = watch::channel(start.clone());

        tokio::spawn(async move {
            let payload = protocol::AgentErrorPayload {
                agent_id: start.agent_id.clone(),
                code: protocol::AgentErrorCode::BackendFailed,
                message: error,
                fatal: true,
            };
            status_handle
                .update(|s| {
                    s.terminated = true;
                    s.turn_completed = true;
                    s.last_error = Some(payload.message.clone());
                    s.activity_counter = s.activity_counter.saturating_add(1);
                })
                .await;

            let mut event_log = Vec::new();
            let mut latest_output = AgentControlLatestOutput::default();
            let mut subscribers = Vec::new();
            append_event(
                &format!("/agent/{}", start.agent_id),
                &mut event_log,
                &mut subscribers,
                FrameKind::AgentError,
                &payload,
            )
            .await;
            loop {
                latest_output
                    .observe_event_log(&event_log)
                    .expect("typed failed-agent replay log must project latest output");
                let Some(command) = rx.recv().await else {
                    break;
                };
                match command {
                    AgentCommand::ResumeReplayBarrier { .. } => {}
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
                        let _ =
                            reply.send(session_history_window(&event_log, before_seq, limit, None));
                    }
                    AgentCommand::ReadActivityHistory {
                        after_seq,
                        max_events,
                        max_bytes,
                        reply,
                    } => {
                        let _ = reply.send(activity_history_snapshot(
                            &event_log, None, after_seq, max_events, max_bytes,
                        ));
                    }
                    AgentCommand::ReadSupervisionContext { reply } => {
                        let _ = reply.send(crate::agent::supervisor::supervision_context_snapshot(
                            &event_log,
                        ));
                    }
                    AgentCommand::ReadUsageSnapshot { reply } => {
                        let _ = reply.send(agent_usage_snapshot_from_log(&start, &event_log));
                    }
                    AgentCommand::Attach { stream, reply } => {
                        let attached = attach_subscriber_with_latest_output(
                            &event_log,
                            None,
                            latest_output.output(),
                            &mut subscribers,
                            stream,
                        );
                        let _ = reply.send(attached);
                    }
                    AgentCommand::SetName { reply, .. } => {
                        let _ = reply.send(false);
                    }
                    AgentCommand::ApplyGeneratedName { reply, .. } => {
                        let _ = reply.send(false);
                    }
                    AgentCommand::Close { reply } => {
                        let _ = reply.send(());
                        break;
                    }
                    AgentCommand::Compact { reply, .. } => {
                        let _ = reply.send(Err("agent is not running".to_owned()));
                    }
                    AgentCommand::CompactIfInactive { accepted, reply, .. } => {
                        let error = "agent is not running".to_owned();
                        let _ = accepted.send(Err(error.clone()));
                        let _ = reply.send(Err(error));
                    }
                    AgentCommand::ReleaseCompaction { reply } => {
                        let _ = reply.send(());
                    }
                    AgentCommand::SendInput(_) => {
                        let rejection = terminal_input_rejected_payload(&start.agent_id);
                        append_event(
                            &format!("/agent/{}", start.agent_id),
                            &mut event_log,
                            &mut subscribers,
                            FrameKind::AgentError,
                            &rejection,
                        )
                        .await;
                    }
                    AgentCommand::Interrupt { reply } => {
                        let _ = reply.send(InterruptOutcome::NotRunning);
                    }
                }
            }
        });

        AgentHandle {
            tx,
            accepting_input: accepting_input_task,
            closing,
            start: start_rx,
        }
    }

    fn assistant_message(content: &str) -> ChatMessage {
        ChatMessage {
            message_id: None,
            timestamp: 1,
            sender: MessageSender::Assistant {
                agent: "mock".to_owned(),
            },
            content: content.to_owned(),
            reasoning: None,
            tool_calls: Vec::new(),
            model_info: None,
            token_usage: None,
            context_breakdown: None,
            images: None,
        }
    }

    fn assistant_message_with_id(message_id: &str, content: &str) -> ChatMessage {
        let mut message = assistant_message(content);
        message.message_id = Some(ChatMessageId(message_id.to_owned()));
        message
    }

    fn metadata_update(message_id: &str, total_tokens: u64) -> ChatEvent {
        ChatEvent::MessageMetadataUpdated(MessageMetadataUpdateData {
            message_id: ChatMessageId(message_id.to_owned()),
            model_info: Some(ModelInfo {
                model: "mock-model".to_owned(),
            }),
            token_usage: Some(MessageTokenUsage::request_and_turn_known(
                token_usage(total_tokens),
                token_usage(total_tokens),
            )),
            context_breakdown: None,
        })
    }

    fn tool_request(tool_call_id: &str) -> ChatEvent {
        ChatEvent::ToolRequest(ToolRequest {
            tool_call_id: tool_call_id.to_owned(),
            tool_name: "run_command".to_owned(),
            tool_type: ToolRequestType::RunCommand {
                command: "echo hi".to_owned(),
                working_directory: "/tmp".to_owned(),
            },
        })
    }

    fn tool_completed(tool_call_id: &str) -> ChatEvent {
        ChatEvent::ToolExecutionCompleted(ToolExecutionCompletedData {
            tool_call_id: tool_call_id.to_owned(),
            tool_name: "run_command".to_owned(),
            tool_result: ToolExecutionResult::RunCommand {
                exit_code: 0,
                stdout: "hi\n".to_owned(),
                stderr: String::new(),
            },
            success: true,
            error: None,
            normalization_failure: None,
        })
    }

    fn token_usage(total_tokens: u64) -> TokenUsage {
        TokenUsage {
            input_tokens: total_tokens / 2,
            output_tokens: total_tokens - (total_tokens / 2),
            total_tokens,
            cached_prompt_tokens: Some(0),
            cache_creation_input_tokens: Some(0),
            reasoning_tokens: Some(0),
        }
    }

    fn scoped_token_usage(total_tokens: u64) -> MessageTokenUsage {
        MessageTokenUsage::request_and_turn_known(
            token_usage(total_tokens),
            token_usage(total_tokens),
        )
    }

    fn stream_start(message_id: &str) -> ChatEvent {
        ChatEvent::StreamStart(StreamStartData {
            message_id: Some(message_id.to_owned()),
            agent: "mock".to_owned(),
            model: None,
        })
    }

    fn stream_end_with_usage(message_id: &str, content: &str, total_tokens: u64) -> ChatEvent {
        let mut message = assistant_message(content);
        message.message_id = Some(ChatMessageId(message_id.to_owned()));
        message.token_usage = Some(MessageTokenUsage::request_and_turn_known(
            token_usage(total_tokens),
            token_usage(total_tokens),
        ));
        ChatEvent::StreamEnd(StreamEndData { message })
    }

    fn observe_stats(
        stats: &mut AgentActivityStatsTracker,
        mut event: ChatEvent,
        source_seq: u64,
        active_stream_text: &str,
    ) -> bool {
        stats.observe_chat_event(&mut event, source_seq, active_stream_text)
    }

    fn test_agent_start(agent_id: &str) -> AgentStartPayload {
        AgentStartPayload {
            agent_id: AgentId(agent_id.to_owned()),
            name: "Test Agent".to_owned(),
            origin: protocol::AgentOrigin::User,
            backend_kind: protocol::BackendKind::Tycode,
            launch_profile_id: None,
            workspace_roots: vec!["/tmp/test".to_owned()],
            custom_agent_id: None,
            team_id: None,
            team_member_id: None,
            project_id: None,
            parent_agent_id: None,
            session_id: None,
            workflow: None,
            created_at_ms: 1,
        }
    }

    fn activity_summary_request() -> GenerateAgentActivitySummaryRequest {
        GenerateAgentActivitySummaryRequest {
            summary_agent_id: AgentId("summary-agent".to_owned()),
            backend_kind: protocol::BackendKind::Claude,
            workspace_roots: vec!["/tmp/test".to_owned()],
            rendered_history: "assistant used a tool".to_owned(),
            previous_summary: None,
            source_from_seq: Some(1),
            source_through_seq: Some(2),
            use_mock_backend: false,
            capacity_tx: tokio::sync::mpsc::unbounded_channel().0,
        }
    }

    #[tokio::test]
    async fn activity_summary_tool_request_does_not_fail_when_text_arrives() {
        let (tx, rx) = mpsc::unbounded_channel();
        tx.send(tool_request("summary-tool"))
            .expect("send tool request");
        tx.send(ChatEvent::StreamDelta(StreamTextDeltaData {
            message_id: Some("summary-message".to_owned()),
            text: "Agent is reviewing the requested files".to_owned(),
        }))
        .expect("send stream delta");
        tx.send(ChatEvent::StreamEnd(StreamEndData {
            message: assistant_message(""),
        }))
        .expect("send stream end");
        drop(tx);
        let mut events = EventStream::new(rx);

        let summary =
            collect_agent_activity_summary_events(&activity_summary_request(), &mut events, 0, 0)
                .await
                .expect("streamed text should satisfy summary generation");

        assert_eq!(summary.text, "Agent is reviewing the requested files");
        assert_eq!(summary.source_from_seq, Some(1));
        assert_eq!(summary.source_through_seq, Some(2));
    }

    #[tokio::test]
    async fn activity_summary_tool_request_empty_stream_end_waits_for_later_text() {
        let (tx, rx) = mpsc::unbounded_channel();
        tx.send(stream_start("empty-segment"))
            .expect("send empty stream start");
        tx.send(tool_request("summary-tool"))
            .expect("send tool request");
        tx.send(ChatEvent::StreamEnd(StreamEndData {
            message: assistant_message(""),
        }))
        .expect("send empty stream end");
        tx.send(stream_start("answer-segment"))
            .expect("send answer stream start");
        tx.send(ChatEvent::StreamEnd(StreamEndData {
            message: assistant_message("Agent finished updating the activity summary"),
        }))
        .expect("send final stream end");
        drop(tx);
        let mut events = EventStream::new(rx);

        let summary =
            collect_agent_activity_summary_events(&activity_summary_request(), &mut events, 0, 0)
                .await
                .expect("later final text should satisfy summary generation");

        assert_eq!(summary.text, "Agent finished updating the activity summary");
    }

    #[tokio::test]
    async fn activity_summary_reasoning_only_stream_end_waits_for_later_answer() {
        let (tx, rx) = mpsc::unbounded_channel();
        tx.send(stream_start("reasoning-segment"))
            .expect("send reasoning stream start");
        tx.send(ChatEvent::StreamReasoningDelta(StreamTextDeltaData {
            message_id: Some("reasoning-segment".to_owned()),
            text: "Thinking through the recent tool activity".to_owned(),
        }))
        .expect("send reasoning delta");
        let mut reasoning_message = assistant_message("");
        reasoning_message.reasoning = Some(ReasoningData {
            text: "Thinking through the recent tool activity".to_owned(),
            tokens: Some(7),
            signature: None,
            blob: None,
        });
        tx.send(ChatEvent::StreamEnd(StreamEndData {
            message: reasoning_message,
        }))
        .expect("send reasoning-only stream end");
        tx.send(stream_start("answer-segment"))
            .expect("send answer stream start");
        tx.send(ChatEvent::StreamEnd(StreamEndData {
            message: assistant_message("Agent is summarizing completed Codex work"),
        }))
        .expect("send final stream end");
        drop(tx);
        let mut events = EventStream::new(rx);

        let summary =
            collect_agent_activity_summary_events(&activity_summary_request(), &mut events, 0, 0)
                .await
                .expect("later final text should satisfy summary generation");

        assert_eq!(summary.text, "Agent is summarizing completed Codex work");
    }

    #[tokio::test]
    async fn activity_summary_tool_request_without_text_is_explicit_error() {
        for include_stream_end in [false, true] {
            let (tx, rx) = mpsc::unbounded_channel();
            tx.send(tool_request("summary-tool"))
                .expect("send tool request");
            if include_stream_end {
                tx.send(ChatEvent::StreamEnd(StreamEndData {
                    message: assistant_message(""),
                }))
                .expect("send empty stream end");
            }
            drop(tx);
            let mut events = EventStream::new(rx);
            let mode = if include_stream_end {
                "empty stream end"
            } else {
                "backend close"
            };

            let error = collect_agent_activity_summary_events(
                &activity_summary_request(),
                &mut events,
                0,
                0,
            )
            .await
            .expect_err("tool-only summary generation should fail");

            assert!(
                error.contains("no usable assistant text"),
                "{mode}: unexpected error: {error}"
            );
            assert!(
                error.contains("attempted 1 tool call(s)"),
                "{mode}: unexpected error: {error}"
            );
            assert!(
                error.contains("run_command (summary-tool)"),
                "{mode}: unexpected error: {error}"
            );
        }
    }

    #[test]
    fn activity_stats_tracks_latest_output_without_stream_start_or_end_clearing() {
        let mut stats = AgentActivityStatsTracker::default();

        assert!(!observe_stats(
            &mut stats,
            ChatEvent::StreamStart(StreamStartData {
                message_id: Some("message-1".to_owned()),
                agent: "mock".to_owned(),
                model: None,
            }),
            0,
            "",
        ));
        assert_eq!(stats.snapshot().last_output_line, None);

        assert!(observe_stats(
            &mut stats,
            ChatEvent::StreamDelta(StreamTextDeltaData {
                message_id: Some("message-1".to_owned()),
                text: "first line".to_owned(),
            }),
            1,
            "first line",
        ));
        assert_eq!(
            stats.snapshot().last_output_line.as_deref(),
            Some("first line")
        );

        let mut empty_end = assistant_message("");
        empty_end.message_id = Some(ChatMessageId("message-1".to_owned()));
        assert!(!observe_stats(
            &mut stats,
            ChatEvent::StreamEnd(StreamEndData { message: empty_end }),
            2,
            "",
        ));
        assert_eq!(
            stats.snapshot().last_output_line.as_deref(),
            Some("first line")
        );

        assert!(!observe_stats(
            &mut stats,
            ChatEvent::StreamStart(StreamStartData {
                message_id: Some("message-2".to_owned()),
                agent: "mock".to_owned(),
                model: None,
            }),
            3,
            "",
        ));
        assert_eq!(
            stats.snapshot().last_output_line.as_deref(),
            Some("first line")
        );

        let mut final_message = assistant_message("second line\nfinal line");
        final_message.message_id = Some(ChatMessageId("message-2".to_owned()));
        assert!(observe_stats(
            &mut stats,
            ChatEvent::StreamEnd(StreamEndData {
                message: final_message
            }),
            4,
            "",
        ));
        assert_eq!(
            stats.snapshot().last_output_line.as_deref(),
            Some("final line")
        );
        assert_eq!(stats.snapshot().source_through_seq, Some(4));
    }

    #[test]
    fn activity_stats_accumulates_reasoning_deltas() {
        let mut stats = AgentActivityStatsTracker::default();

        assert!(!observe_stats(&mut stats, stream_start("message-1"), 0, ""));
        for (seq, text) in ["Thi", "nking", " abo", "ut x"].into_iter().enumerate() {
            observe_stats(
                &mut stats,
                ChatEvent::StreamReasoningDelta(StreamTextDeltaData {
                    message_id: Some("message-1".to_owned()),
                    text: text.to_owned(),
                }),
                seq as u64 + 1,
                "",
            );
        }
        assert_eq!(
            stats.snapshot().last_output_line.as_deref(),
            Some("Thinking about x")
        );

        assert!(!observe_stats(&mut stats, stream_start("message-2"), 5, ""));
        assert!(observe_stats(
            &mut stats,
            ChatEvent::StreamReasoningDelta(StreamTextDeltaData {
                message_id: Some("message-2".to_owned()),
                text: "Fresh thought".to_owned(),
            }),
            6,
            "",
        ));
        assert_eq!(
            stats.snapshot().last_output_line.as_deref(),
            Some("Fresh thought")
        );
    }

    #[test]
    fn activity_stats_counts_unique_tool_requests() {
        let mut stats = AgentActivityStatsTracker::default();

        assert!(observe_stats(&mut stats, tool_request("tool-1"), 0, ""));
        assert_eq!(stats.snapshot().tool_calls, 1);
        assert!(!observe_stats(&mut stats, tool_request("tool-1"), 1, ""));
        assert_eq!(stats.snapshot().tool_calls, 1);
        assert!(observe_stats(&mut stats, tool_request("tool-2"), 2, ""));
        assert_eq!(stats.snapshot().tool_calls, 2);
        assert_eq!(stats.snapshot().source_through_seq, Some(2));
    }

    #[test]
    fn activity_stats_token_metadata_replaces_by_message_id() {
        let mut stats = AgentActivityStatsTracker::default();
        let mut message = assistant_message("done");
        message.message_id = Some(ChatMessageId("message-1".to_owned()));
        message.token_usage = Some(scoped_token_usage(10));

        assert!(observe_stats(
            &mut stats,
            ChatEvent::MessageAdded(message),
            0,
            ""
        ));
        assert_eq!(stats.snapshot().token_usage.total_tokens, 10);
        assert!(observe_stats(
            &mut stats,
            metadata_update("message-1", 22),
            1,
            ""
        ));
        assert_eq!(stats.snapshot().token_usage.total_tokens, 22);
        assert_eq!(stats.snapshot().source_through_seq, Some(1));
    }

    #[test]
    fn codex_activity_stats_use_model_requests_not_chat_records() {
        let mut stats = AgentActivityStatsTracker::for_backend(protocol::BackendKind::Codex);
        for sequence in 0..180 {
            let mut message = assistant_message("intermediate Codex record");
            message.message_id = Some(ChatMessageId(format!("message-{sequence}")));
            let mut event = ChatEvent::MessageAdded(message);
            stats.observe_chat_event(&mut event, sequence, "");
            let ChatEvent::MessageAdded(message) = event else {
                unreachable!();
            };
            assert!(message.token_usage.is_none());
        }

        let first = ModelRequestTokenUsage {
            request_id: ModelRequestId {
                turn_id: ModelTurnId("turn-1".to_owned()),
                sequence: 0,
            },
            request: token_usage(40),
            turn: token_usage(40),
            cumulative: token_usage(1_000),
            model_context_window: Some(400_000),
        };
        assert!(stats.observe_model_request_token_usage(first, 180));

        let second = ModelRequestTokenUsage {
            request_id: ModelRequestId {
                turn_id: ModelTurnId("turn-1".to_owned()),
                sequence: 1,
            },
            request: token_usage(25),
            turn: token_usage(65),
            cumulative: token_usage(1_025),
            model_context_window: Some(400_000),
        };
        assert!(stats.observe_model_request_token_usage(second, 181));

        let (usage, _) = stats.usage_snapshot();
        let TaskTokenUsageScope::Known { usage } = usage else {
            panic!("Codex request usage should be fully known");
        };
        assert_eq!(usage.total_tokens, 1_025);
        assert_eq!(stats.token_usage_by_source.len(), 2);
    }

    #[test]
    fn activity_stats_stamps_cumulative_scope_without_changing_request_scope() {
        let mut stats = AgentActivityStatsTracker::default();
        let mut message = assistant_message("done");
        message.message_id = Some(ChatMessageId("message-1".to_owned()));
        message.token_usage = Some(MessageTokenUsage::request_and_turn_known(
            token_usage(7),
            token_usage(11),
        ));
        let mut event = ChatEvent::MessageAdded(message);

        assert!(stats.observe_chat_event(&mut event, 0, ""));
        let ChatEvent::MessageAdded(message) = event else {
            panic!("expected MessageAdded")
        };
        let usage = message.token_usage.expect("message token usage");
        assert_eq!(
            usage.request.known_usage().map(|usage| usage.total_tokens),
            Some(7)
        );
        assert_eq!(
            usage.turn.known_usage().map(|usage| usage.total_tokens),
            Some(11)
        );
        assert_eq!(
            usage
                .cumulative
                .known_usage()
                .map(|usage| usage.total_tokens),
            Some(11)
        );
        assert_eq!(stats.snapshot().token_usage.total_tokens, 11);
    }

    #[test]
    fn activity_stats_does_not_synthesize_known_cumulative_from_incomplete_sources() {
        let mut stats = AgentActivityStatsTracker::default();
        let mut missing = assistant_message("missing usage");
        missing.message_id = Some(ChatMessageId("missing-usage".to_owned()));
        observe_stats(&mut stats, ChatEvent::MessageAdded(missing), 0, "");

        let mut known = assistant_message("known usage");
        known.message_id = Some(ChatMessageId("known-usage".to_owned()));
        known.token_usage = Some(scoped_token_usage(10));
        let mut event = ChatEvent::MessageAdded(known);

        assert!(stats.observe_chat_event(&mut event, 1, ""));
        let ChatEvent::MessageAdded(message) = event else {
            panic!("expected MessageAdded")
        };
        let usage = message.token_usage.expect("message token usage");
        assert_eq!(
            usage.turn.known_usage().map(|usage| usage.total_tokens),
            Some(10)
        );
        assert!(matches!(
            usage.cumulative,
            TokenUsageScope::Unavailable {
                reason: TokenUsageUnavailableReason::BackendDidNotReport
            }
        ));
        assert_eq!(stats.snapshot().token_usage.total_tokens, 10);

        let (usage, _) = stats.usage_snapshot();
        match usage {
            TaskTokenUsageScope::Partial {
                usage,
                unavailable_count,
                reasons,
            } => {
                assert_eq!(usage.total_tokens, 10);
                assert_eq!(unavailable_count, 1);
                assert_eq!(
                    reasons,
                    vec![TaskTokenUsageUnavailableReason::BackendDidNotReport]
                );
            }
            other => panic!("expected partial usage snapshot, got {other:?}"),
        }
    }

    #[test]
    fn activity_stats_preserves_request_only_usage_without_inventing_turn() {
        let mut stats = AgentActivityStatsTracker::default();
        let mut message = assistant_message("intermediate");
        message.message_id = Some(ChatMessageId("message-1".to_owned()));
        message.token_usage = Some(MessageTokenUsage::request_known(token_usage(7)));
        let mut event = ChatEvent::MessageAdded(message);

        assert!(stats.observe_chat_event(&mut event, 0, ""));
        let ChatEvent::MessageAdded(message) = event else {
            panic!("expected MessageAdded")
        };
        let usage = message.token_usage.expect("message token usage");
        assert_eq!(
            usage.request.known_usage().map(|usage| usage.total_tokens),
            Some(7)
        );
        assert!(usage.turn.known_usage().is_none());
        assert!(usage.cumulative.known_usage().is_none());
        assert_eq!(stats.snapshot().token_usage, TokenUsage::default());
    }

    #[test]
    fn activity_stats_preserves_explicit_ambiguous_cumulative_scope() {
        let mut stats = AgentActivityStatsTracker::default();
        let mut message = assistant_message("current turn");
        message.message_id = Some(ChatMessageId("message-ambiguous-cumulative".to_owned()));
        let mut token_usage =
            MessageTokenUsage::request_and_turn_known(token_usage(7), token_usage(11));
        token_usage.cumulative = TokenUsageScope::Unavailable {
            reason: TokenUsageUnavailableReason::ProviderScopeAmbiguous,
        };
        message.token_usage = Some(token_usage);
        let mut event = ChatEvent::MessageAdded(message);

        assert!(stats.observe_chat_event(&mut event, 0, ""));
        let ChatEvent::MessageAdded(message) = event else {
            panic!("expected MessageAdded")
        };
        assert!(matches!(
            message.token_usage.expect("message usage").cumulative,
            TokenUsageScope::Unavailable {
                reason: TokenUsageUnavailableReason::ProviderScopeAmbiguous
            }
        ));
        assert_eq!(stats.snapshot().token_usage.total_tokens, 11);
    }

    #[test]
    fn usage_snapshot_empty_reports_no_completed_assistant_turn() {
        let stats = AgentActivityStatsTracker::default();

        let (usage, _) = stats.usage_snapshot();

        assert!(matches!(
            usage,
            TaskTokenUsageScope::Unavailable {
                reason: TaskTokenUsageUnavailableReason::NoAssistantTurnCompleted
            }
        ));
    }

    #[test]
    fn usage_snapshot_all_unavailable_reports_unavailable_reason() {
        let mut stats = AgentActivityStatsTracker::default();
        let mut message = assistant_message("missing usage");
        message.message_id = Some(ChatMessageId("missing-usage".to_owned()));

        observe_stats(&mut stats, ChatEvent::MessageAdded(message), 0, "");
        let (usage, _) = stats.usage_snapshot();

        assert!(matches!(
            usage,
            TaskTokenUsageScope::Unavailable {
                reason: TaskTokenUsageUnavailableReason::BackendDidNotReport
            }
        ));
        assert_eq!(stats.snapshot().token_usage, TokenUsage::default());
    }

    #[test]
    fn usage_snapshot_mixed_known_and_unavailable_sources_is_partial() {
        let mut stats = AgentActivityStatsTracker::default();
        let mut missing = assistant_message("missing usage");
        missing.message_id = Some(ChatMessageId("missing-usage".to_owned()));
        observe_stats(&mut stats, ChatEvent::MessageAdded(missing), 0, "");

        let mut known = assistant_message("known usage");
        known.message_id = Some(ChatMessageId("known-usage".to_owned()));
        known.token_usage = Some(scoped_token_usage(10));
        observe_stats(&mut stats, ChatEvent::MessageAdded(known), 1, "");
        let (usage, _) = stats.usage_snapshot();

        match usage {
            TaskTokenUsageScope::Partial {
                usage,
                unavailable_count,
                reasons,
            } => {
                assert_eq!(usage.total_tokens, 10);
                assert_eq!(usage.input_tokens, Some(5));
                assert_eq!(usage.output_tokens, Some(5));
                assert_eq!(unavailable_count, 1);
                assert_eq!(
                    reasons,
                    vec![TaskTokenUsageUnavailableReason::BackendDidNotReport]
                );
            }
            other => panic!("expected partial mixed-source usage snapshot, got {other:?}"),
        }
        assert_eq!(stats.snapshot().token_usage.total_tokens, 10);
    }

    #[test]
    fn usage_snapshot_from_log_preserves_mixed_source_partial_usage() {
        let start = test_agent_start("log-usage-agent");
        let stream = StreamPath("/agent/log-usage-agent".to_owned());
        let mut missing = assistant_message("missing usage");
        missing.message_id = Some(ChatMessageId("missing-usage".to_owned()));
        let mut known = assistant_message("known usage");
        known.message_id = Some(ChatMessageId("known-usage".to_owned()));
        known.token_usage = Some(scoped_token_usage(10).with_cumulative(token_usage(10)));
        let events = [
            ChatEvent::MessageAdded(missing),
            ChatEvent::MessageAdded(known),
        ];
        let event_log = events
            .iter()
            .enumerate()
            .map(|(seq, event)| {
                protocol::Envelope::from_payload(
                    stream.clone(),
                    FrameKind::ChatEvent,
                    seq as u64,
                    event,
                )
                .expect("serialize ChatEvent")
            })
            .collect::<Vec<_>>();

        let snapshot = agent_usage_snapshot_from_log(&start, &event_log);

        match snapshot.usage {
            TaskTokenUsageScope::Partial {
                usage,
                unavailable_count,
                reasons,
            } => {
                assert_eq!(usage.total_tokens, 10);
                assert_eq!(unavailable_count, 1);
                assert_eq!(
                    reasons,
                    vec![TaskTokenUsageUnavailableReason::BackendDidNotReport]
                );
            }
            other => panic!("expected partial log usage snapshot, got {other:?}"),
        }
    }

    #[test]
    fn usage_snapshot_from_log_uses_latest_stats_total_as_numeric_floor() {
        let start = test_agent_start("truncated-log-usage-agent");
        let stream = StreamPath("/agent/truncated-log-usage-agent".to_owned());
        let stats = AgentActivityStatsPayload {
            agent_id: start.agent_id.clone(),
            stats: AgentActivityStats {
                last_output_line: None,
                tool_calls: 0,
                token_usage: token_usage(30),
                token_usage_total_only: None,
                source_through_seq: Some(7),
            },
        };
        let mut missing = assistant_message("visible missing usage");
        missing.message_id = Some(ChatMessageId("visible-missing-usage".to_owned()));
        let mut known = assistant_message("visible known usage");
        known.message_id = Some(ChatMessageId("visible-known-usage".to_owned()));
        known.token_usage = Some(scoped_token_usage(10).with_cumulative(token_usage(10)));
        let event_log = vec![
            protocol::Envelope::from_payload(
                stream.clone(),
                FrameKind::AgentActivityStats,
                0,
                &stats,
            )
            .expect("serialize AgentActivityStats"),
            protocol::Envelope::from_payload(
                stream.clone(),
                FrameKind::ChatEvent,
                1,
                &ChatEvent::MessageAdded(missing),
            )
            .expect("serialize missing ChatEvent"),
            protocol::Envelope::from_payload(
                stream,
                FrameKind::ChatEvent,
                2,
                &ChatEvent::MessageAdded(known),
            )
            .expect("serialize known ChatEvent"),
        ];

        let snapshot = agent_usage_snapshot_from_log(&start, &event_log);

        match snapshot.usage {
            TaskTokenUsageScope::Partial {
                usage,
                unavailable_count,
                reasons,
            } => {
                assert_eq!(usage.total_tokens, 30);
                assert_eq!(unavailable_count, 1);
                assert_eq!(
                    reasons,
                    vec![TaskTokenUsageUnavailableReason::BackendDidNotReport]
                );
            }
            other => panic!("expected partial log usage snapshot, got {other:?}"),
        }
    }

    #[test]
    fn usage_snapshot_from_log_combines_visible_unavailable_with_stats_total() {
        let start = test_agent_start("truncated-log-unavailable-agent");
        let stream = StreamPath("/agent/truncated-log-unavailable-agent".to_owned());
        let stats = AgentActivityStatsPayload {
            agent_id: start.agent_id.clone(),
            stats: AgentActivityStats {
                last_output_line: None,
                tool_calls: 0,
                token_usage: token_usage(30),
                token_usage_total_only: None,
                source_through_seq: Some(7),
            },
        };
        let mut missing = assistant_message("visible missing usage");
        missing.message_id = Some(ChatMessageId("visible-missing-usage".to_owned()));
        let event_log = vec![
            protocol::Envelope::from_payload(
                stream.clone(),
                FrameKind::AgentActivityStats,
                0,
                &stats,
            )
            .expect("serialize AgentActivityStats"),
            protocol::Envelope::from_payload(
                stream,
                FrameKind::ChatEvent,
                1,
                &ChatEvent::MessageAdded(missing),
            )
            .expect("serialize missing ChatEvent"),
        ];

        let snapshot = agent_usage_snapshot_from_log(&start, &event_log);

        match snapshot.usage {
            TaskTokenUsageScope::Partial {
                usage,
                unavailable_count,
                reasons,
            } => {
                assert_eq!(usage.total_tokens, 30);
                assert_eq!(unavailable_count, 1);
                assert_eq!(
                    reasons,
                    vec![TaskTokenUsageUnavailableReason::BackendDidNotReport]
                );
            }
            other => panic!("expected partial log usage snapshot, got {other:?}"),
        }
    }

    #[test]
    fn usage_snapshot_from_log_uses_stats_fallback_without_chat_usage() {
        let start = test_agent_start("legacy-stats-agent");
        let payload = AgentActivityStatsPayload {
            agent_id: start.agent_id.clone(),
            stats: AgentActivityStats {
                last_output_line: None,
                tool_calls: 0,
                token_usage: token_usage(12),
                token_usage_total_only: None,
                source_through_seq: Some(7),
            },
        };
        let event_log = vec![
            protocol::Envelope::from_payload(
                StreamPath("/agent/legacy-stats-agent".to_owned()),
                FrameKind::AgentActivityStats,
                0,
                &payload,
            )
            .expect("serialize AgentActivityStats"),
        ];

        let snapshot = agent_usage_snapshot_from_log(&start, &event_log);

        match snapshot.usage {
            TaskTokenUsageScope::Known { usage } => {
                assert_eq!(usage.total_tokens, 12);
            }
            other => panic!("expected stats-only fallback usage, got {other:?}"),
        }
    }

    #[test]
    fn usage_snapshot_from_log_preserves_total_only_activity_snapshot() {
        let mut start = test_agent_start("total-only-stats-agent");
        start.backend_kind = BackendKind::Claude;
        let payload = AgentActivityStatsPayload {
            agent_id: start.agent_id.clone(),
            stats: AgentActivityStats {
                token_usage_total_only: Some(53),
                source_through_seq: Some(4),
                ..AgentActivityStats::default()
            },
        };
        let event_log = vec![
            protocol::Envelope::from_payload(
                StreamPath("/agent/total-only-stats-agent".to_owned()),
                FrameKind::AgentActivityStats,
                0,
                &payload,
            )
            .expect("serialize total-only stats"),
        ];

        let snapshot = agent_usage_snapshot_from_log(&start, &event_log);
        let TaskTokenUsageScope::Known { usage } = snapshot.usage else {
            panic!("total-only activity snapshot should remain known");
        };
        assert_eq!(usage.total_tokens, 53);
        assert_eq!(usage.input_tokens, None);
        assert_eq!(usage.output_tokens, None);
    }

    #[test]
    fn activity_stats_uses_event_seq_for_unidentified_token_usage() {
        let mut stats = AgentActivityStatsTracker::default();
        let mut first = assistant_message("first");
        first.token_usage = Some(scoped_token_usage(6));
        let mut second = assistant_message("second");
        second.token_usage = Some(scoped_token_usage(8));

        assert!(observe_stats(
            &mut stats,
            ChatEvent::MessageAdded(first),
            7,
            ""
        ));
        assert!(observe_stats(
            &mut stats,
            ChatEvent::MessageAdded(second),
            8,
            ""
        ));
        assert_eq!(stats.snapshot().token_usage.total_tokens, 14);
        assert_eq!(stats.snapshot().source_through_seq, Some(8));
    }

    #[test]
    fn claude_total_only_usage_is_known_without_invented_components() {
        let mut stats = AgentActivityStatsTracker::for_backend(BackendKind::Claude);
        assert!(stats.observe_total_only_token_usage(31, 4));

        let (usage, _) = stats.usage_snapshot();
        let TaskTokenUsageScope::Known { usage } = usage else {
            panic!("aggregate provider usage should be known");
        };
        assert_eq!(usage.total_tokens, 31);
        assert_eq!(usage.input_tokens, None);
        assert_eq!(usage.output_tokens, None);
        assert_eq!(stats.snapshot().token_usage_total_only, Some(31));
    }

    #[tokio::test]
    async fn relay_activity_stats_accumulate_subagent_turn_usage() {
        let dir = tempfile::tempdir().expect("tempdir");
        let session_store_path = dir.path().join("sessions.json");
        let session_store = Arc::new(Mutex::new(
            SessionStore::load(session_store_path).expect("load session store"),
        ));
        let mut start = test_agent_start("relay-stats-agent");
        start.backend_kind = BackendKind::Claude;
        let (status_handle, _status_rx) = AgentStatusHandle::new();
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (model_usage_tx, model_usage_rx) = mpsc::unbounded_channel();
        let (total_usage_tx, total_usage_rx) = mpsc::unbounded_channel();
        let handle = spawn_relay_agent_actor(
            start.agent_id.clone(),
            start,
            RelayEventReceivers {
                events: event_rx,
                model_usage: model_usage_rx,
                total_usage: total_usage_rx,
            },
            session_store,
            SessionId("relay-session".to_owned()),
            status_handle,
        );
        let (output_tx, mut output_rx) = mpsc::unbounded_channel();
        assert!(handle.attach(replay_stream(output_tx)).await);
        let _ = recv_agent_bootstrap_events(&mut output_rx, "relay stats bootstrap").await;

        event_tx
            .send(stream_start("message-1"))
            .expect("relay event channel should be open");
        event_tx
            .send(stream_end_with_usage("message-1", "first", 10))
            .expect("relay event channel should be open");
        event_tx
            .send(stream_start("message-2"))
            .expect("relay event channel should be open");
        event_tx
            .send(stream_end_with_usage("message-2", "second", 7))
            .expect("relay event channel should be open");
        model_usage_tx
            .send(ModelRequestTokenUsage {
                request_id: ModelRequestId {
                    turn_id: ModelTurnId("claude-child-turn".to_owned()),
                    sequence: 1,
                },
                request: token_usage(17),
                turn: token_usage(17),
                cumulative: token_usage(17),
                model_context_window: None,
            })
            .expect("relay model usage channel should be open");
        total_usage_tx
            .send(31)
            .expect("relay total-only usage channel should be open");

        let stats = timeout(Duration::from_secs(1), async {
            loop {
                let event = output_rx
                    .recv()
                    .await
                    .expect("relay output stream should stay open");
                if event.kind == FrameKind::AgentActivityStats
                    && let Ok(payload) = event.parse_payload::<AgentActivityStatsPayload>()
                    && payload.stats.token_usage.total_tokens == 17
                    && payload.stats.token_usage_total_only == Some(31)
                    && payload.stats.last_output_line.as_deref() == Some("second")
                {
                    return payload.stats;
                }
            }
        })
        .await
        .expect("relay token stats should be emitted");

        assert_eq!(stats.token_usage.total_tokens, 17);
        assert_eq!(stats.token_usage.input_tokens, 8);
        assert_eq!(stats.token_usage.output_tokens, 9);
        assert_eq!(stats.token_usage_total_only, Some(31));
        assert_eq!(stats.last_output_line.as_deref(), Some("second"));
        drop(event_tx);
        drop(model_usage_tx);
        drop(total_usage_tx);
        assert!(handle.close().await);
    }

    #[tokio::test]
    async fn relay_preserves_codex_model_request_usage_in_live_and_replay_stats() {
        let dir = tempfile::tempdir().expect("tempdir");
        let session_store = Arc::new(Mutex::new(
            SessionStore::load(dir.path().join("sessions.json")).expect("load session store"),
        ));
        let mut start = test_agent_start("relay-codex-usage");
        start.backend_kind = BackendKind::Codex;
        let (status_handle, _status_rx) = AgentStatusHandle::new();
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (model_usage_tx, model_usage_rx) = mpsc::unbounded_channel();
        let (_total_usage_tx, total_usage_rx) = mpsc::unbounded_channel();
        let handle = spawn_relay_agent_actor(
            start.agent_id.clone(),
            start,
            RelayEventReceivers {
                events: event_rx,
                model_usage: model_usage_rx,
                total_usage: total_usage_rx,
            },
            session_store,
            SessionId("relay-codex-session".to_owned()),
            status_handle,
        );
        let (live_tx, mut live_rx) = mpsc::unbounded_channel();
        assert!(handle.attach(replay_stream(live_tx)).await);
        let _ = recv_agent_bootstrap_events(&mut live_rx, "Codex relay bootstrap").await;

        model_usage_tx
            .send(ModelRequestTokenUsage {
                request_id: ModelRequestId {
                    turn_id: ModelTurnId("child-turn".to_owned()),
                    sequence: 1,
                },
                request: token_usage(11),
                turn: token_usage(11),
                cumulative: token_usage(11),
                model_context_window: Some(200_000),
            })
            .expect("relay model usage channel should be open");

        let live_stats = timeout(Duration::from_secs(1), async {
            loop {
                let event = live_rx.recv().await.expect("live relay stream");
                if event.kind == FrameKind::AgentActivityStats
                    && let Ok(payload) = event.parse_payload::<AgentActivityStatsPayload>()
                    && payload.stats.token_usage.total_tokens == 11
                {
                    return payload.stats;
                }
            }
        })
        .await
        .expect("live model usage stats");
        assert_eq!(live_stats.token_usage.total_tokens, 11);

        let (replay_tx, mut replay_rx) = mpsc::unbounded_channel();
        assert!(handle.attach(replay_stream(replay_tx)).await);
        let replay = recv_agent_bootstrap_events(&mut replay_rx, "Codex usage replay").await;
        let replay_stats = replay
            .iter()
            .find_map(|event| match event {
                AgentBootstrapEvent::AgentActivityStats(payload) => Some(&payload.stats),
                _ => None,
            })
            .expect("replay activity stats");
        assert_eq!(replay_stats.token_usage.total_tokens, 11);

        drop(event_tx);
        drop(model_usage_tx);
        assert!(handle.close().await);
    }

    #[tokio::test]
    async fn bootstrap_includes_current_activity_stats_snapshot() {
        let canonical_stream = "/agent/stats-agent";
        let mut event_log = Vec::new();
        let mut subscribers = Vec::new();
        let start = test_agent_start("stats-agent");
        append_event(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            FrameKind::AgentStart,
            &start,
        )
        .await;
        upsert_activity_stats_snapshot(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &start.agent_id,
            AgentActivityStats {
                last_output_line: Some("latest output".to_owned()),
                tool_calls: 3,
                token_usage: token_usage(30),
                token_usage_total_only: None,
                source_through_seq: Some(9),
            },
        )
        .await;

        let (tx, mut rx) = mpsc::unbounded_channel();
        attach_subscriber(&event_log, None, &mut subscribers, replay_stream(tx));

        let events = recv_agent_bootstrap_events(&mut rx, "agent activity stats bootstrap").await;
        let stats = events
            .iter()
            .find_map(|event| match event {
                AgentBootstrapEvent::AgentActivityStats(payload) => Some(payload),
                _ => None,
            })
            .expect("AgentBootstrap should include AgentActivityStats");
        assert_eq!(stats.agent_id, start.agent_id);
        assert_eq!(
            stats.stats.last_output_line.as_deref(),
            Some("latest output")
        );
        assert_eq!(stats.stats.tool_calls, 3);
        assert_eq!(stats.stats.token_usage.total_tokens, 30);
        assert_eq!(stats.stats.source_through_seq, Some(9));
    }

    async fn append_completed_tool_turn(
        canonical_stream: &str,
        event_log: &mut Vec<protocol::Envelope>,
        subscribers: &mut Vec<Stream>,
        replay_state: &mut AgentReplayState,
        message_id: &str,
        tool_call_id: &str,
        content: &str,
    ) {
        append_chat_event(
            canonical_stream,
            event_log,
            subscribers,
            replay_state,
            &ChatEvent::TypingStatusChanged(true),
        )
        .await;
        append_chat_event(
            canonical_stream,
            event_log,
            subscribers,
            replay_state,
            &ChatEvent::StreamStart(StreamStartData {
                message_id: Some(message_id.to_owned()),
                agent: "mock".to_owned(),
                model: Some("mock-model".to_owned()),
            }),
        )
        .await;
        append_chat_event(
            canonical_stream,
            event_log,
            subscribers,
            replay_state,
            &tool_request(tool_call_id),
        )
        .await;
        append_chat_event(
            canonical_stream,
            event_log,
            subscribers,
            replay_state,
            &tool_completed(tool_call_id),
        )
        .await;
        let mut message = assistant_message(content);
        message.message_id = Some(ChatMessageId(message_id.to_owned()));
        append_chat_event(
            canonical_stream,
            event_log,
            subscribers,
            replay_state,
            &ChatEvent::StreamEnd(StreamEndData { message }),
        )
        .await;
        append_chat_event(
            canonical_stream,
            event_log,
            subscribers,
            replay_state,
            &ChatEvent::TypingStatusChanged(false),
        )
        .await;
    }

    async fn recv_agent_bootstrap_events(
        rx: &mut mpsc::UnboundedReceiver<protocol::Envelope>,
        context: &str,
    ) -> Vec<AgentBootstrapEvent> {
        let env = timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for {context}"))
            .unwrap_or_else(|| panic!("stream closed before {context}"));
        assert_eq!(env.kind, FrameKind::AgentBootstrap);
        env.parse_payload::<AgentBootstrapPayload>()
            .unwrap_or_else(|_| panic!("failed to parse {context}"))
            .events
    }

    fn replay_stream(tx: mpsc::UnboundedSender<protocol::Envelope>) -> Stream {
        Stream::new(StreamPath("/agent/replay-instance".to_owned()), tx)
    }

    #[test]
    fn generated_name_sanitizer_accepts_valid_name() {
        assert_eq!(
            sanitize_generated_agent_name("  \"fix login flow\" ").unwrap(),
            "Fix Login Flow"
        );
    }

    #[tokio::test]
    async fn resumed_agent_start_is_idle_before_follow_up() {
        let (status_handle, _status_rx) = AgentStatusHandle::new();
        status_handle
            .update(|status| record_agent_started(status, true))
            .await;

        let status = status_handle.snapshot().await;
        assert!(status.started);
        assert!(!status.is_thinking);
        assert!(status.turn_completed);
        assert!(!status.is_active());
        assert_eq!(status.status(), AgentControlStatus::Idle);
    }

    #[tokio::test]
    async fn resumed_history_bootstrap_ends_with_authoritative_idle() {
        let canonical_stream = "/agent/resumed-agent";
        let (status_handle, _status_rx) = AgentStatusHandle::new();
        let mut event_log = Vec::new();
        let mut subscribers = Vec::new();
        let mut replay_state = AgentReplayState::default();
        append_chat_event(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
            &ChatEvent::MessageAdded(ChatMessage {
                message_id: Some(ChatMessageId("prior-user-message".to_owned())),
                timestamp: 1,
                sender: MessageSender::User,
                content: "prior request".to_owned(),
                reasoning: None,
                tool_calls: Vec::new(),
                model_info: None,
                token_usage: None,
                context_breakdown: None,
                images: None,
            }),
        )
        .await;
        publish_resumed_agent_idle(
            &status_handle,
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
        )
        .await;

        let (tx, mut rx) = mpsc::unbounded_channel();
        attach_subscriber(
            &event_log,
            Some(&replay_state),
            &mut subscribers,
            replay_stream(tx),
        );
        let events = recv_agent_bootstrap_events(&mut rx, "resumed bootstrap").await;
        assert!(matches!(
            events.as_slice(),
            [
                AgentBootstrapEvent::ChatEvent(ChatEvent::MessageAdded(_)),
                AgentBootstrapEvent::ChatEvent(ChatEvent::TypingStatusChanged(false)),
            ]
        ));
    }

    #[tokio::test]
    async fn accepted_turn_marks_completed_agent_active() {
        let (status_handle, _status_rx) = AgentStatusHandle::new();
        status_handle
            .update(|status| {
                status.started = true;
                status.turn_completed = true;
            })
            .await;

        mark_agent_turn_active(&status_handle).await;

        let status = status_handle.snapshot().await;
        assert!(status.is_thinking);
        assert!(!status.turn_completed);
        assert!(status.is_active());
        assert_eq!(status.status(), AgentControlStatus::Thinking);
    }

    #[tokio::test]
    async fn relay_idle_marker_makes_native_child_inactive() {
        let dir = tempfile::tempdir().expect("tempdir");
        let session_store = Arc::new(Mutex::new(
            SessionStore::load(dir.path().join("sessions.json")).expect("load session store"),
        ));
        let start = test_agent_start("relay-idle-child");
        let (status_handle, _status_rx) = AgentStatusHandle::new();
        let status = status_handle.clone();
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (_model_tx, model_rx) = mpsc::unbounded_channel();
        let (_total_tx, total_rx) = mpsc::unbounded_channel();
        let handle = spawn_relay_agent_actor(
            start.agent_id.clone(),
            start,
            RelayEventReceivers {
                events: event_rx,
                model_usage: model_rx,
                total_usage: total_rx,
            },
            session_store,
            SessionId("relay-idle-session".to_owned()),
            status_handle,
        );

        event_tx
            .send(ChatEvent::TypingStatusChanged(false))
            .expect("relay event channel should be open");
        timeout(Duration::from_secs(1), async {
            loop {
                let snapshot = status.snapshot().await;
                if snapshot.turn_completed {
                    assert!(!snapshot.is_thinking);
                    assert!(!snapshot.is_active());
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("relay idle should update child status");

        drop(event_tx);
        assert!(handle.close().await);
    }

    #[tokio::test]
    async fn generated_name_timeout_retains_fallback_without_agent_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let session_store = Arc::new(Mutex::new(
            SessionStore::load(dir.path().join("sessions.json")).expect("load session store"),
        ));
        let mut current_start = test_agent_start("name-timeout-agent");
        current_start.name = "Inspect Backend".to_owned();
        let original = current_start.clone();
        let (start_tx, _start_rx) = watch::channel(current_start.clone());
        let mut pending_alias = None;
        let mut event_log = Vec::new();
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let mut subscribers = vec![Stream::new(
            StreamPath("/agent/name-timeout-agent".to_owned()),
            event_tx,
        )];

        let applied = apply_generated_agent_name(
            AgentNameChangeContext {
                session_store: &session_store,
                session_id: None,
                pending_alias: &mut pending_alias,
                current_start: &mut current_start,
                start_tx: &start_tx,
                event_log: &mut event_log,
                subscribers: &mut subscribers,
            },
            Err("agent name generation timed out after 30 seconds".to_owned()),
        )
        .await;

        assert!(!applied);
        assert_eq!(current_start.name, original.name);
        assert!(pending_alias.is_none());
        assert!(event_log.is_empty());
        assert!(event_rx.try_recv().is_err());
    }

    #[test]
    fn legacy_native_collaboration_replay_is_sanitized_but_other_tools_are_not() {
        let mut events = vec![
            ChatEvent::ToolRequest(protocol::ToolRequest {
                tool_call_id: "claude-call".to_owned(),
                tool_name: "Task".to_owned(),
                tool_type: ToolRequestType::Other {
                    args: serde_json::json!({
                        "prompt": "Inspect authentication", "description": "Auth investigator",
                        "subagent_type": "Explore", "resume": "claude-session-secret",
                        "output_file": "/private/tmp/claude-child-output"
                    }),
                },
            }),
            ChatEvent::ToolExecutionCompleted(ToolExecutionCompletedData {
                tool_call_id: "claude-call".to_owned(),
                tool_name: "Task".to_owned(),
                tool_result: ToolExecutionResult::Other {
                    result: serde_json::json!({
                        "agentId": "claude-provider-agent-id",
                        "output_file": "/private/tmp/claude-child-output",
                        "content": "provider control prose"
                    }),
                },
                success: true,
                error: None,
                normalization_failure: None,
            }),
            ChatEvent::ToolExecutionCompleted(ToolExecutionCompletedData {
                tool_call_id: "codex-call".to_owned(),
                tool_name: "spawnAgent".to_owned(),
                tool_result: ToolExecutionResult::Other {
                    result: serde_json::json!({
                        "type": "collabAgentToolCall", "tool": "spawnAgent",
                        "senderThreadId": "codex-parent-thread-id",
                        "receiverThreadId": "codex-child-thread-id",
                        "output_file": "/tmp/codex-child-output",
                        "content": "codex transport control prose"
                    }),
                },
                success: true,
                error: None,
                normalization_failure: None,
            }),
        ];
        let replay_log = events
            .iter()
            .enumerate()
            .map(|(seq, event)| {
                replay_envelope("/agent/legacy", seq as u64, FrameKind::ChatEvent, event)
            })
            .collect::<Vec<_>>();
        let replay = session_history_entries_from_log(&replay_log)
            .into_iter()
            .map(|(_, event)| event)
            .collect::<Vec<_>>();
        events = replay;
        assert!(matches!(
            &events[0],
            ChatEvent::ToolRequest(protocol::ToolRequest {
                tool_type: ToolRequestType::AgentSpawn { prompt, name }, ..
            }) if prompt.as_deref() == Some("Inspect authentication")
                && name.as_deref() == Some("Auth investigator")
        ));
        let encoded = serde_json::to_string(&events).expect("serialize projected replay");
        for private in [
            "claude-session-secret",
            "claude-provider-agent-id",
            "codex-parent-thread-id",
            "codex-child-thread-id",
            "/private/tmp",
            "/tmp/codex",
            "provider control prose",
            "codex transport control prose",
        ] {
            assert!(
                !encoded.contains(private),
                "replay leaked {private}: {encoded}"
            );
        }

        let mut ordinary = ChatEvent::ToolExecutionCompleted(ToolExecutionCompletedData {
            tool_call_id: "ordinary".to_owned(),
            tool_name: "custom_report".to_owned(),
            tool_result: ToolExecutionResult::Other {
                result: serde_json::json!({
                    "content": "ordinary tool payload",
                    "output_file": "/tmp/user-requested-report"
                }),
            },
            success: true,
            error: None,
            normalization_failure: None,
        });
        let original = serde_json::to_value(&ordinary).expect("serialize ordinary tool");
        project_legacy_native_collaboration_event(&mut ordinary);
        assert_eq!(
            serde_json::to_value(&ordinary).expect("serialize projected ordinary tool"),
            original
        );
    }

    #[test]
    fn generated_name_setup_selects_backend_inference_mode() {
        let config = agent_name_generation_spawn_config();

        assert!(config.startup_mcp_servers.is_empty());
        assert_eq!(config.execution_mode, BackendExecutionMode::InferenceOnly);
        assert!(config.session_settings.is_none());
        assert!(config.backend_config.0.is_empty());
        assert_eq!(
            config.resolved_spawn_config.access_mode,
            protocol::BackendAccessMode::ReadOnly
        );
        assert_eq!(
            config.resolved_spawn_config.tool_policy,
            protocol::ToolPolicy::AllowList { tools: Vec::new() }
        );
    }

    fn generated_name_stream_end(content: &str, reasoning: Option<&str>) -> ChatEvent {
        ChatEvent::StreamEnd(StreamEndData {
            message: ChatMessage {
                message_id: Some(ChatMessageId("generated-name".to_owned())),
                timestamp: 0,
                sender: MessageSender::Assistant {
                    agent: "codex".to_owned(),
                },
                content: content.to_owned(),
                reasoning: reasoning.map(|text| ReasoningData {
                    text: text.to_owned(),
                    tokens: None,
                    signature: None,
                    blob: None,
                }),
                tool_calls: Vec::new(),
                model_info: None,
                token_usage: None,
                context_breakdown: None,
                images: None,
            },
        })
    }

    #[tokio::test]
    async fn generated_name_waits_past_reasoning_only_stream_end() {
        let (tx, rx) = mpsc::unbounded_channel();
        tx.send(generated_name_stream_end(
            "",
            Some("Choosing a concise title."),
        ))
        .expect("reasoning-only StreamEnd");
        tx.send(ChatEvent::StreamDelta(StreamTextDeltaData {
            message_id: Some("generated-name".to_owned()),
            text: "Fix Login Flow".to_owned(),
        }))
        .expect("assistant name delta");
        tx.send(generated_name_stream_end("Fix Login Flow", None))
            .expect("assistant StreamEnd");
        drop(tx);
        let mut events = EventStream::new(rx);

        assert_eq!(
            collect_agent_name_events(&mut events).await.unwrap(),
            "Fix Login Flow"
        );
    }

    /// Pinned to the captured Tycode wire: SetRootAgent completes with its own
    /// typing true → typing false cycle before the naming turn starts. That
    /// setup cycle must not be read as "turn completed without a response" —
    /// it previously aborted naming and discarded the generated name.
    #[tokio::test]
    async fn generated_name_ignores_setup_typing_cycle_before_turn() {
        let (tx, rx) = mpsc::unbounded_channel();
        tx.send(ChatEvent::TypingStatusChanged(true))
            .expect("setup typing start");
        tx.send(ChatEvent::TypingStatusChanged(false))
            .expect("setup typing completion");
        tx.send(ChatEvent::TypingStatusChanged(true))
            .expect("turn typing start");
        tx.send(ChatEvent::StreamDelta(StreamTextDeltaData {
            message_id: Some("generated-name".to_owned()),
            text: "Tyde Backend QA".to_owned(),
        }))
        .expect("assistant name delta");
        tx.send(generated_name_stream_end("Tyde Backend QA", None))
            .expect("assistant StreamEnd");
        drop(tx);
        let mut events = EventStream::new(rx);

        assert_eq!(
            collect_agent_name_events(&mut events).await.unwrap(),
            // sanitize_generated_agent_name title-cases each word.
            "Tyde Backend Qa"
        );
    }

    #[tokio::test]
    async fn generated_name_empty_completion_fails_without_prompt_fallback() {
        let (tx, rx) = mpsc::unbounded_channel();
        tx.send(generated_name_stream_end("", Some("No final answer.")))
            .expect("reasoning-only StreamEnd");
        tx.send(ChatEvent::TypingStatusChanged(false))
            .expect("turn completion");
        let mut events = EventStream::new(rx);

        let error = collect_agent_name_events(&mut events)
            .await
            .expect_err("empty completion must fail");
        assert_eq!(
            error,
            "agent name generator turn completed before producing a final response"
        );
    }

    // The 2-4 word rule is a prompt instruction, not an acceptance gate: a
    // model that answers "Greeting" for "hi" produced a perfectly good name,
    // and rejecting it used to discard the generation (and, before naming
    // went async, fail the whole spawn). Any answer with usable words is
    // accepted; overlong answers are truncated instead of rejected.
    #[tokio::test]
    async fn generated_name_accepts_single_word_answer() {
        let (tx, rx) = mpsc::unbounded_channel();
        tx.send(generated_name_stream_end("Greeting", None))
            .expect("single-word assistant StreamEnd");
        drop(tx);
        let mut events = EventStream::new(rx);

        assert_eq!(
            collect_agent_name_events(&mut events).await.unwrap(),
            "Greeting"
        );
    }

    #[test]
    fn generated_name_sanitizer_truncates_overlong_answer() {
        assert_eq!(
            sanitize_generated_agent_name("fix the login flow for the mobile app").unwrap(),
            "Fix The Login Flow"
        );
    }

    #[test]
    fn generated_name_sanitizer_rejects_answer_with_no_usable_words() {
        let error = sanitize_generated_agent_name("\"\u{201c}\u{201d}\"").expect_err(
            "an answer that sanitizes to nothing must fail rather than produce an empty name",
        );
        assert!(error.contains("no usable words") || error.contains("empty"));
    }

    #[test]
    fn mock_name_uses_default_words_when_prompt_has_no_name_words() {
        assert_eq!(generate_mock_name("!!!").unwrap(), "New Agent Task");
    }

    #[tokio::test]
    async fn bootstrap_tail_starts_at_message_boundary_with_tool_history() {
        let canonical_stream = "/agent/replay-agent";
        let mut event_log = Vec::new();
        let mut replay_state = AgentReplayState::default();
        let mut subscribers = Vec::new();

        append_completed_tool_turn(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
            "message-0",
            "tool-0",
            "tool history 0",
        )
        .await;
        append_completed_tool_turn(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
            "message-1",
            "tool-1",
            "tool history 1",
        )
        .await;
        for index in 2..=15 {
            let mut message = assistant_message(&format!("history {index}"));
            message.message_id = Some(ChatMessageId(format!("message-{index}")));
            append_chat_event(
                canonical_stream,
                &mut event_log,
                &mut subscribers,
                &mut replay_state,
                &ChatEvent::MessageAdded(message),
            )
            .await;
        }

        let (tx, mut rx) = mpsc::unbounded_channel();
        attach_subscriber(
            &event_log,
            Some(&replay_state),
            &mut subscribers,
            replay_stream(tx),
        );

        let events = recv_agent_bootstrap_events(&mut rx, "agent bootstrap").await;
        let before_seq = events
            .iter()
            .find_map(|event| match event {
                AgentBootstrapEvent::HasPriorHistory {
                    message_count: 1,
                    before_seq,
                } => Some(*before_seq),
                _ => None,
            })
            .expect("bootstrap should gate the single older message");
        let chat_events = events
            .iter()
            .filter_map(|event| match event {
                AgentBootstrapEvent::ChatEvent(event) => Some(event),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            chat_events
                .iter()
                .filter(|event| matches!(event, ChatEvent::MessageAdded(_)))
                .count(),
            15
        );
        match chat_events.as_slice() {
            [
                ChatEvent::MessageAdded(message),
                ChatEvent::ToolRequest(request),
                ChatEvent::ToolExecutionCompleted(completion),
                ..,
            ] => {
                assert_eq!(message.content, "tool history 1");
                assert_eq!(request.tool_call_id, "tool-1");
                assert_eq!(completion.tool_call_id, "tool-1");
            }
            other => panic!("tail should start with a complete tool-bearing turn, got {other:?}"),
        }

        let older_page = session_history_window(&event_log, Some(before_seq), 10, None);
        assert!(!older_page.has_more_before);
        match older_page.events.as_slice() {
            [
                ChatEvent::ToolExecutionCompleted(completion),
                ChatEvent::ToolRequest(request),
                ChatEvent::MessageAdded(message),
            ] => {
                assert_eq!(completion.tool_call_id, "tool-0");
                assert_eq!(request.tool_call_id, "tool-0");
                assert_eq!(message.content, "tool history 0");
            }
            other => {
                panic!("older page should contain the complete older tool turn, got {other:?}")
            }
        }
    }

    #[tokio::test]
    async fn session_history_window_pages_by_message_boundary_with_tool_history() {
        let canonical_stream = "/agent/replay-agent";
        let mut event_log = Vec::new();
        let mut replay_state = AgentReplayState::default();
        let mut subscribers = Vec::new();

        append_completed_tool_turn(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
            "message-0",
            "tool-0",
            "tool history 0",
        )
        .await;
        append_completed_tool_turn(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
            "message-1",
            "tool-1",
            "tool history 1",
        )
        .await;

        let newest_page = session_history_window(&event_log, None, 1, None);
        assert!(newest_page.has_more_before);
        let newest_cursor = newest_page
            .oldest_seq
            .expect("newest page should expose a cursor");
        match newest_page.events.as_slice() {
            [
                ChatEvent::ToolExecutionCompleted(completion),
                ChatEvent::ToolRequest(request),
                ChatEvent::MessageAdded(message),
            ] => {
                assert_eq!(completion.tool_call_id, "tool-1");
                assert_eq!(request.tool_call_id, "tool-1");
                assert_eq!(message.content, "tool history 1");
            }
            other => panic!("newest page should contain one complete tool turn, got {other:?}"),
        }

        let older_page = session_history_window(&event_log, Some(newest_cursor), 1, None);
        assert!(!older_page.has_more_before);
        match older_page.events.as_slice() {
            [
                ChatEvent::ToolExecutionCompleted(completion),
                ChatEvent::ToolRequest(request),
                ChatEvent::MessageAdded(message),
            ] => {
                assert_eq!(completion.tool_call_id, "tool-0");
                assert_eq!(request.tool_call_id, "tool-0");
                assert_eq!(message.content, "tool history 0");
            }
            other => panic!("older page should contain one complete tool turn, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn bootstrap_completed_stream_replays_post_end_tool_events_after_stream_end() {
        let canonical_stream = "/agent/replay-agent";
        let mut event_log = Vec::new();
        let mut replay_state = AgentReplayState::default();
        let mut subscribers = Vec::new();

        append_chat_event(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
            &ChatEvent::TypingStatusChanged(true),
        )
        .await;
        append_chat_event(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
            &ChatEvent::StreamStart(StreamStartData {
                message_id: Some("message-1".to_owned()),
                agent: "mock".to_owned(),
                model: Some("mock-model".to_owned()),
            }),
        )
        .await;
        append_chat_event(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
            &tool_request("pre-end-tool"),
        )
        .await;
        let mut message = assistant_message("finished before tools");
        message.message_id = Some(ChatMessageId("message-1".to_owned()));
        append_chat_event(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
            &ChatEvent::StreamEnd(StreamEndData { message }),
        )
        .await;

        let (active_tx, mut active_rx) = mpsc::unbounded_channel();
        attach_subscriber(
            &event_log,
            Some(&replay_state),
            &mut subscribers,
            replay_stream(active_tx),
        );
        let active_events = recv_agent_bootstrap_events(&mut active_rx, "active tool bootstrap")
            .await
            .into_iter()
            .filter_map(|event| match event {
                AgentBootstrapEvent::ChatEvent(event) => Some(event),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(matches!(
            active_events.as_slice(),
            [
                ChatEvent::TypingStatusChanged(true),
                ChatEvent::StreamStart(_),
                ChatEvent::ToolRequest(_),
                ChatEvent::StreamEnd(_),
            ]
        ));

        append_chat_event(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
            &tool_completed("pre-end-tool"),
        )
        .await;
        append_chat_event(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
            &tool_request("post-end-tool"),
        )
        .await;

        let (tx, mut rx) = mpsc::unbounded_channel();
        attach_subscriber(
            &event_log,
            Some(&replay_state),
            &mut subscribers,
            replay_stream(tx),
        );

        let events = recv_agent_bootstrap_events(&mut rx, "agent bootstrap").await;
        assert!(
            events
                .iter()
                .all(|event| !matches!(event, AgentBootstrapEvent::HasPriorHistory { .. })),
            "filtered completed-stream history must not create a false prior-history gate: {events:?}"
        );
        let chat_events = events
            .iter()
            .filter_map(|event| match event {
                AgentBootstrapEvent::ChatEvent(event) => Some(event),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(
            chat_events
                .iter()
                .all(|event| !matches!(event, ChatEvent::MessageAdded(_))),
            "completed stream history should be replayed through active events only: {chat_events:?}"
        );
        let stream_end_index = chat_events
            .iter()
            .position(|event| matches!(event, ChatEvent::StreamEnd(_)))
            .expect("active replay should include StreamEnd");
        let pre_request_index = chat_events
            .iter()
            .position(|event| matches!(event, ChatEvent::ToolRequest(request) if request.tool_call_id == "pre-end-tool"))
            .expect("active replay should include the pre-end tool request");
        let pre_completion_index = chat_events
            .iter()
            .position(|event| matches!(event, ChatEvent::ToolExecutionCompleted(completion) if completion.tool_call_id == "pre-end-tool"))
            .expect("active replay should include the post-end tool completion");
        let post_request_index = chat_events
            .iter()
            .position(|event| matches!(event, ChatEvent::ToolRequest(request) if request.tool_call_id == "post-end-tool"))
            .expect("active replay should include the post-end tool request");
        assert!(
            pre_request_index < stream_end_index,
            "pre-end request should replay before StreamEnd: {chat_events:?}"
        );
        assert!(
            stream_end_index < pre_completion_index,
            "post-end completion should replay after StreamEnd: {chat_events:?}"
        );
        assert!(
            stream_end_index < post_request_index,
            "post-end request should replay after StreamEnd: {chat_events:?}"
        );
    }

    #[tokio::test]
    async fn reconnect_persists_prior_tool_completion_outside_later_active_stream() {
        let canonical_stream = "/agent/replay-agent";
        let mut event_log = Vec::new();
        let mut replay_state = AgentReplayState::default();
        let mut subscribers = Vec::new();

        append_chat_event(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
            &ChatEvent::TypingStatusChanged(true),
        )
        .await;
        append_chat_event(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
            &ChatEvent::StreamStart(StreamStartData {
                message_id: Some("message-before-background".to_owned()),
                agent: "codex".to_owned(),
                model: Some("gpt-5.6-luna".to_owned()),
            }),
        )
        .await;
        let mut first_message = assistant_message("starting");
        first_message.message_id = Some(ChatMessageId("message-before-background".to_owned()));
        append_chat_event(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
            &ChatEvent::StreamEnd(StreamEndData {
                message: first_message,
            }),
        )
        .await;
        append_chat_event(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
            &tool_request("background-tool"),
        )
        .await;
        append_chat_event(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
            &ChatEvent::StreamStart(StreamStartData {
                message_id: Some("message-while-background-runs".to_owned()),
                agent: "codex".to_owned(),
                model: Some("gpt-5.6-luna".to_owned()),
            }),
        )
        .await;
        append_chat_event(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
            &tool_completed("background-tool"),
        )
        .await;

        let active_events = replay_state.active_stream_events();
        assert!(active_events.iter().any(|event| {
            matches!(
                event,
                ChatEvent::StreamStart(start)
                    if start.message_id.as_deref() == Some("message-while-background-runs")
            )
        }));
        assert!(
            active_events.iter().all(|event| {
                !matches!(
                    event,
                    ChatEvent::ToolExecutionCompleted(completion)
                        if completion.tool_call_id == "background-tool"
                )
            }),
            "the earlier completion must not be replayed as a tool owned by the later active message"
        );

        let persisted_chat_events = event_log
            .iter()
            .filter(|envelope| envelope.kind == FrameKind::ChatEvent)
            .filter_map(|envelope| envelope.parse_payload::<ChatEvent>().ok())
            .collect::<Vec<_>>();
        let request_index = persisted_chat_events
            .iter()
            .position(|event| {
                matches!(
                    event,
                    ChatEvent::ToolRequest(request)
                        if request.tool_call_id == "background-tool"
                )
            })
            .expect("background request persists for history replay");
        let completion_index = persisted_chat_events
            .iter()
            .position(|event| {
                matches!(
                    event,
                    ChatEvent::ToolExecutionCompleted(completion)
                        if completion.tool_call_id == "background-tool"
                )
            })
            .expect("background completion persists for history replay");
        assert!(request_index < completion_index);
    }

    #[tokio::test]
    async fn bootstrap_completed_stream_replays_metadata_updated_stream_end() {
        let canonical_stream = "/agent/replay-agent";
        let mut event_log = Vec::new();
        let mut replay_state = AgentReplayState::default();
        let mut subscribers = Vec::new();

        append_chat_event(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
            &ChatEvent::TypingStatusChanged(true),
        )
        .await;
        append_chat_event(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
            &ChatEvent::StreamStart(StreamStartData {
                message_id: Some("message-1".to_owned()),
                agent: "mock".to_owned(),
                model: Some("mock-model".to_owned()),
            }),
        )
        .await;
        append_chat_event(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
            &ChatEvent::StreamEnd(StreamEndData {
                message: assistant_message_with_id("message-1", "metadata arrives later"),
            }),
        )
        .await;
        append_chat_event(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
            &metadata_update("message-1", 42),
        )
        .await;

        let (tx, mut rx) = mpsc::unbounded_channel();
        attach_subscriber(
            &event_log,
            Some(&replay_state),
            &mut subscribers,
            replay_stream(tx),
        );

        let events = recv_agent_bootstrap_events(&mut rx, "agent bootstrap").await;
        let chat_events = events
            .iter()
            .filter_map(|event| match event {
                AgentBootstrapEvent::ChatEvent(event) => Some(event),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(
            chat_events
                .iter()
                .all(|event| !matches!(event, ChatEvent::MessageAdded(_))),
            "metadata-updated completed message should not be duplicated in tail: {chat_events:?}"
        );
        let stream_end = chat_events
            .iter()
            .find_map(|event| match event {
                ChatEvent::StreamEnd(data) => Some(data),
                _ => None,
            })
            .expect("active replay should include StreamEnd");
        assert_eq!(
            stream_end.message.message_id,
            Some(ChatMessageId("message-1".to_owned()))
        );
        assert_eq!(
            stream_end
                .message
                .token_usage
                .as_ref()
                .and_then(|usage| usage.turn.known_usage())
                .map(|usage| usage.total_tokens),
            Some(42)
        );
    }

    #[tokio::test]
    async fn bootstrap_completed_stream_filter_keeps_later_history_entries() {
        let canonical_stream = "/agent/replay-agent";
        let mut event_log = Vec::new();
        let mut replay_state = AgentReplayState::default();
        let mut subscribers = Vec::new();

        append_chat_event(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
            &ChatEvent::TypingStatusChanged(true),
        )
        .await;
        append_chat_event(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
            &ChatEvent::StreamStart(StreamStartData {
                message_id: Some("message-1".to_owned()),
                agent: "mock".to_owned(),
                model: Some("mock-model".to_owned()),
            }),
        )
        .await;
        append_chat_event(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
            &ChatEvent::StreamDelta(StreamTextDeltaData {
                message_id: Some("message-1".to_owned()),
                text: "hello".to_owned(),
            }),
        )
        .await;
        let mut message = assistant_message("hello");
        message.message_id = Some(ChatMessageId("message-1".to_owned()));
        append_chat_event(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
            &ChatEvent::StreamEnd(StreamEndData { message }),
        )
        .await;
        append_chat_event(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
            &ChatEvent::TaskUpdate(TaskList {
                title: "after stream".to_owned(),
                tasks: Vec::new(),
            }),
        )
        .await;

        let (tx, mut rx) = mpsc::unbounded_channel();
        attach_subscriber(
            &event_log,
            Some(&replay_state),
            &mut subscribers,
            replay_stream(tx),
        );

        let events = recv_agent_bootstrap_events(&mut rx, "agent bootstrap").await;
        let chat_events = events
            .iter()
            .filter_map(|event| match event {
                AgentBootstrapEvent::ChatEvent(event) => Some(event),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(
            chat_events.iter().all(|event| !matches!(
                event,
                ChatEvent::MessageAdded(message) if message.content == "hello"
            )),
            "completed stream should not duplicate compacted MessageAdded: {chat_events:?}"
        );
        assert!(
            chat_events.iter().any(|event| matches!(
                event,
                ChatEvent::TaskUpdate(tasks) if tasks.title == "after stream"
            )),
            "history entries after StreamEnd should remain in the bootstrap tail: {chat_events:?}"
        );
        assert!(matches!(
            chat_events.as_slice(),
            [
                ChatEvent::TaskUpdate(_),
                ChatEvent::TypingStatusChanged(true),
                ChatEvent::StreamStart(_),
                ChatEvent::StreamDelta(_),
                ChatEvent::StreamEnd(_),
            ]
        ));

        let task_seq = event_log
            .iter()
            .find_map(|envelope| {
                if envelope.kind != FrameKind::ChatEvent {
                    return None;
                }
                match envelope.parse_payload::<ChatEvent>().ok()? {
                    ChatEvent::TaskUpdate(tasks) if tasks.title == "after stream" => {
                        Some(envelope.seq)
                    }
                    _ => None,
                }
            })
            .expect("expected TaskUpdate in replay log");
        let prior_page =
            session_history_window(&event_log, Some(task_seq), 10, Some(&replay_state));
        assert!(!prior_page.has_more_before);
        assert!(
            prior_page.events.is_empty(),
            "filtered active completed message must not be fetchable as older history: {:?}",
            prior_page.events
        );
    }

    #[tokio::test]
    async fn replay_compacts_completed_stream_into_message_added() {
        let canonical_stream = "/agent/replay-agent";
        let mut event_log = Vec::new();
        let mut replay_state = AgentReplayState::default();
        let mut subscribers = Vec::new();

        append_chat_event(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
            &ChatEvent::TypingStatusChanged(true),
        )
        .await;
        append_chat_event(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
            &ChatEvent::StreamStart(StreamStartData {
                message_id: Some("message-1".to_owned()),
                agent: "mock".to_owned(),
                model: Some("mock-model".to_owned()),
            }),
        )
        .await;
        for text in ["hello", " ", "world"] {
            append_chat_event(
                canonical_stream,
                &mut event_log,
                &mut subscribers,
                &mut replay_state,
                &ChatEvent::StreamDelta(StreamTextDeltaData {
                    message_id: Some("message-1".to_owned()),
                    text: text.to_owned(),
                }),
            )
            .await;
        }
        append_chat_event(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
            &ChatEvent::StreamEnd(StreamEndData {
                message: assistant_message_with_id("message-1", "hello world"),
            }),
        )
        .await;
        append_chat_event(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
            &metadata_update("message-1", 42),
        )
        .await;
        append_chat_event(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
            &ChatEvent::TypingStatusChanged(false),
        )
        .await;

        let (tx, mut rx) = mpsc::unbounded_channel();
        attach_subscriber(
            &event_log,
            Some(&replay_state),
            &mut subscribers,
            replay_stream(tx),
        );

        let mut events = recv_agent_bootstrap_events(&mut rx, "agent bootstrap")
            .await
            .into_iter();
        match events.next() {
            Some(AgentBootstrapEvent::ChatEvent(ChatEvent::MessageAdded(message))) => {
                assert_eq!(message.content, "hello world");
                assert_eq!(
                    message.message_id,
                    Some(ChatMessageId("message-1".to_owned()))
                );
                assert_eq!(
                    known_turn_usage(&message.token_usage).map(|usage| usage.total_tokens),
                    Some(42)
                );
            }
            other => panic!("expected bootstrap MessageAdded tail, got {other:?}"),
        }
        assert!(events.next().is_none());
        assert!(
            timeout(Duration::from_millis(50), rx.recv()).await.is_err(),
            "bootstrap tail should stay inside AgentBootstrap"
        );

        let window = session_history_window(&event_log, None, 10, None);
        assert!(!window.has_more_before);
        assert!(window.oldest_seq.is_some());
        match window.events.as_slice() {
            [ChatEvent::MessageAdded(message)] => {
                assert_eq!(message.content, "hello world");
                assert_eq!(
                    message.message_id,
                    Some(ChatMessageId("message-1".to_owned()))
                );
                assert_eq!(
                    known_turn_usage(&message.token_usage).map(|usage| usage.total_tokens),
                    Some(42)
                );
            }
            other => panic!("expected one compacted history message, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn replay_coalesces_tool_progress_latest_wins() {
        let canonical_stream = "/agent/replay-agent";
        let mut event_log = Vec::new();
        let mut replay_state = AgentReplayState::default();
        let mut subscribers = Vec::new();

        let progress = |tokens: u64| {
            ChatEvent::ToolProgress(protocol::ToolProgressData {
                tool_call_id: "toolu_wf".to_owned(),
                tool_name: "Workflow".to_owned(),
                update: protocol::ToolProgressUpdate::Workflow(protocol::WorkflowRunState {
                    workflow_name: "wfprobe".to_owned(),
                    description: None,
                    script: None,
                    status: protocol::WorkflowRunStatus::Running,
                    summary: None,
                    total_tokens: tokens,
                    tool_uses: 0,
                    duration_ms: 0,
                    agents: vec![],
                }),
            })
        };

        // Outside any stream (the common case: workflow progress arrives
        // between turns), then a later one that must replace it in place.
        for tokens in [100, 200, 300] {
            append_chat_event(
                canonical_stream,
                &mut event_log,
                &mut subscribers,
                &mut replay_state,
                &progress(tokens),
            )
            .await;
        }
        append_chat_event(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
            &ChatEvent::TypingStatusChanged(false),
        )
        .await;

        let progress_envelopes: Vec<&protocol::Envelope> = event_log
            .iter()
            .filter(|envelope| {
                envelope.kind == FrameKind::ChatEvent
                    && matches!(
                        envelope.parse_payload::<ChatEvent>(),
                        Ok(ChatEvent::ToolProgress(_))
                    )
            })
            .collect();
        assert_eq!(
            progress_envelopes.len(),
            1,
            "N progress events coalesce to one envelope"
        );
        let Ok(ChatEvent::ToolProgress(data)) = progress_envelopes[0].parse_payload::<ChatEvent>()
        else {
            panic!("expected ToolProgress payload");
        };
        let protocol::ToolProgressUpdate::Workflow(state) = data.update else {
            panic!("expected Workflow update");
        };
        assert_eq!(state.total_tokens, 300, "latest snapshot wins");
        // Replaced in place: progress keeps its original position before
        // the later TypingStatusChanged.
        assert!(progress_envelopes[0].seq < event_log.last().unwrap().seq);
    }

    #[tokio::test]
    async fn output_events_since_returns_metadata_update_after_message_seq() {
        let canonical_stream = "/agent/replay-agent";
        let mut event_log = Vec::new();
        let mut replay_state = AgentReplayState::default();
        let mut subscribers = Vec::new();

        append_chat_event(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
            &ChatEvent::TypingStatusChanged(true),
        )
        .await;
        append_chat_event(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
            &ChatEvent::StreamStart(StreamStartData {
                message_id: Some("message-1".to_owned()),
                agent: "mock".to_owned(),
                model: Some("mock-model".to_owned()),
            }),
        )
        .await;
        append_chat_event(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
            &ChatEvent::StreamDelta(StreamTextDeltaData {
                message_id: Some("message-1".to_owned()),
                text: "hello world".to_owned(),
            }),
        )
        .await;
        append_chat_event(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
            &ChatEvent::StreamEnd(StreamEndData {
                message: assistant_message_with_id("message-1", "hello world"),
            }),
        )
        .await;

        let message_seq = event_log
            .iter()
            .find_map(|event| match event.parse_payload::<ChatEvent>().ok()? {
                ChatEvent::MessageAdded(message)
                    if message.message_id == Some(ChatMessageId("message-1".to_owned())) =>
                {
                    Some(event.seq)
                }
                _ => None,
            })
            .expect("expected replay MessageAdded");

        append_chat_event(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
            &metadata_update("message-1", 42),
        )
        .await;

        let events = output_events_since(&event_log, Some(message_seq), 10);
        assert_eq!(events.len(), 1);
        assert!(events[0].seq > message_seq);
        match events[0]
            .parse_payload::<ChatEvent>()
            .expect("failed to parse output event")
        {
            ChatEvent::MessageMetadataUpdated(update) => {
                assert_eq!(update.message_id, ChatMessageId("message-1".to_owned()));
                assert_eq!(
                    known_turn_usage(&update.token_usage).map(|usage| usage.total_tokens),
                    Some(42)
                );
            }
            other => panic!("expected MessageMetadataUpdated, got {other:?}"),
        }
    }

    #[test]
    fn latest_output_state_does_not_fall_back_past_empty_message() {
        let canonical_stream = "/agent/replay-agent";
        let prior = replay_envelope(
            canonical_stream,
            1,
            FrameKind::ChatEvent,
            &ChatEvent::MessageAdded(assistant_message("prior visible answer")),
        );
        let latest = replay_envelope(
            canonical_stream,
            2,
            FrameKind::ChatEvent,
            &ChatEvent::MessageAdded(assistant_message("")),
        );
        let metadata = replay_envelope(
            canonical_stream,
            3,
            FrameKind::ChatEvent,
            &metadata_update("message-2", 42),
        );

        let mut state = AgentControlLatestOutput::default();
        let output = current_latest_output(&mut state, &[prior, latest, metadata])
            .expect("typed output records should project");
        assert_eq!(output, AgentControlOutput::Empty);
    }

    #[tokio::test]
    async fn replay_preserves_active_stream_as_aggregated_deltas() {
        let canonical_stream = "/agent/replay-agent";
        let mut event_log = Vec::new();
        let mut replay_state = AgentReplayState::default();
        let mut subscribers = Vec::new();

        append_chat_event(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
            &ChatEvent::TypingStatusChanged(true),
        )
        .await;
        append_chat_event(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
            &ChatEvent::StreamStart(StreamStartData {
                message_id: Some("message-1".to_owned()),
                agent: "mock".to_owned(),
                model: Some("mock-model".to_owned()),
            }),
        )
        .await;
        for text in ["alpha", " beta"] {
            append_chat_event(
                canonical_stream,
                &mut event_log,
                &mut subscribers,
                &mut replay_state,
                &ChatEvent::StreamDelta(StreamTextDeltaData {
                    message_id: Some("message-1".to_owned()),
                    text: text.to_owned(),
                }),
            )
            .await;
        }

        let (tx, mut rx) = mpsc::unbounded_channel();
        attach_subscriber(
            &event_log,
            Some(&replay_state),
            &mut subscribers,
            replay_stream(tx),
        );

        let mut events = recv_agent_bootstrap_events(&mut rx, "agent bootstrap")
            .await
            .into_iter();
        assert!(matches!(
            events.next(),
            Some(AgentBootstrapEvent::ChatEvent(
                ChatEvent::TypingStatusChanged(true)
            ))
        ));
        assert!(matches!(
            events.next(),
            Some(AgentBootstrapEvent::ChatEvent(ChatEvent::StreamStart(..)))
        ));
        match events.next() {
            Some(AgentBootstrapEvent::ChatEvent(ChatEvent::StreamDelta(delta))) => {
                assert_eq!(delta.message_id, Some("message-1".to_owned()));
                assert_eq!(delta.text, "alpha beta");
            }
            other => panic!("expected aggregated StreamDelta, got {other:?}"),
        }
        assert!(events.next().is_none());
        assert!(
            timeout(Duration::from_millis(50), rx.recv()).await.is_err(),
            "active replay should collapse historical deltas into one delta"
        );
    }

    #[tokio::test]
    async fn replay_preserves_server_generated_identity_without_rederiving_it() {
        let canonical_stream = "/agent/replay-agent";
        let mut event_log = Vec::new();
        let mut replay_state = AgentReplayState::default();
        let mut subscribers = Vec::new();
        let identity = ServerGeneratedChatMessageIdentity {
            origin: ServerGeneratedChatMessageIdOrigin::LegacyReplay,
            stream_epoch: 9,
            item_ordinal: 1,
        };
        let message_id = identity.message_id();

        append_chat_event(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
            &ChatEvent::StreamStart(StreamStartData {
                message_id: Some(message_id.0.clone()),
                agent: "mock".to_owned(),
                model: None,
            }),
        )
        .await;
        append_chat_event(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
            &ChatEvent::StreamEnd(StreamEndData {
                message: assistant_message_with_id(&message_id.0, "legacy response"),
            }),
        )
        .await;

        let (tx, mut rx) = mpsc::unbounded_channel();
        attach_subscriber(
            &event_log,
            Some(&replay_state),
            &mut subscribers,
            replay_stream(tx),
        );

        let bootstrap = recv_agent_bootstrap_events(&mut rx, "generated identity bootstrap").await;
        let replayed = bootstrap.into_iter().find_map(|event| match event {
            AgentBootstrapEvent::ChatEvent(ChatEvent::MessageAdded(message)) => message.message_id,
            _ => None,
        });
        assert_eq!(replayed, Some(identity.message_id()));
    }

    #[tokio::test]
    async fn replay_rejects_foreign_delta_without_rebinding_or_raw_id_leakage() {
        let canonical_stream = "/agent/replay-agent";
        let mut event_log = Vec::new();
        let mut replay_state = AgentReplayState::default();
        let mut subscribers = Vec::new();

        append_chat_event(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
            &stream_start("message-1"),
        )
        .await;
        append_chat_event(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
            &ChatEvent::StreamDelta(StreamTextDeltaData {
                message_id: Some("foreign-provider-item-secret".to_owned()),
                text: "foreign".to_owned(),
            }),
        )
        .await;

        let active = replay_state
            .active_stream
            .as_ref()
            .expect("foreign delta must not clear the active stream");
        assert_eq!(active.message_id, ChatMessageId("message-1".to_owned()));
        assert!(active.text.is_empty());
        let logged = event_log
            .iter()
            .map(|envelope| envelope.payload.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(logged.contains("foreign active message id"));
        assert!(!logged.contains("foreign-provider-item-secret"));

        append_chat_event(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
            &ChatEvent::StreamEnd(StreamEndData {
                message: assistant_message_with_id("foreign-end-secret", "foreign end"),
            }),
        )
        .await;
        assert_eq!(
            replay_state
                .active_stream
                .as_ref()
                .expect("foreign end must not close the active stream")
                .message_id,
            ChatMessageId("message-1".to_owned())
        );
        let logged = event_log
            .iter()
            .map(|envelope| envelope.payload.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(logged.contains("mismatched end message id"));
        assert!(!logged.contains("foreign-end-secret"));

        append_chat_event(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
            &ChatEvent::StreamDelta(StreamTextDeltaData {
                message_id: Some("message-1".to_owned()),
                text: "accepted".to_owned(),
            }),
        )
        .await;
        append_chat_event(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
            &ChatEvent::StreamEnd(StreamEndData {
                message: assistant_message_with_id("message-1", "accepted"),
            }),
        )
        .await;

        assert_eq!(
            replay_state
                .completed_stream
                .as_ref()
                .expect("matching end completes the original stream")
                .end
                .message
                .message_id,
            Some(ChatMessageId("message-1".to_owned()))
        );
    }

    #[tokio::test]
    async fn gated_foreign_delta_is_rejected_before_stats_or_active_text_mutate() {
        let canonical_stream = "/agent/replay-agent";
        let mut event_log = Vec::new();
        let mut replay_state = AgentReplayState::default();
        let mut subscribers = Vec::new();
        append_chat_event(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
            &stream_start("message-1"),
        )
        .await;

        let mut event = ChatEvent::StreamDelta(StreamTextDeltaData {
            message_id: Some("foreign-id".to_owned()),
            text: "must not reach derived state".to_owned(),
        });
        let mut stats = AgentActivityStatsTracker::default();
        let mut active_stream_text = "accepted text".to_owned();
        let mut activity_event_seq = 7;
        ingest_gated_replay_event(
            &mut event,
            canonical_stream,
            &AgentId("replay-agent".to_owned()),
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
            &mut stats,
            &mut active_stream_text,
            &mut activity_event_seq,
        )
        .await;

        assert_eq!(active_stream_text, "accepted text");
        assert_eq!(activity_event_seq, 7);
        assert!(event_log.iter().all(|envelope| {
            !envelope
                .payload
                .to_string()
                .contains("must not reach derived state")
        }));
        assert!(event_log.iter().any(|envelope| {
            envelope
                .payload
                .to_string()
                .contains("foreign active message id")
        }));
    }

    #[tokio::test]
    async fn replay_rejects_duplicate_terminal_stream_identity() {
        let canonical_stream = "/agent/replay-agent";
        let mut event_log = Vec::new();
        let mut replay_state = AgentReplayState::default();
        let mut subscribers = Vec::new();

        append_chat_event(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
            &stream_start("message-1"),
        )
        .await;
        append_chat_event(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
            &ChatEvent::StreamEnd(StreamEndData {
                message: assistant_message_with_id("message-1", "complete"),
            }),
        )
        .await;
        append_chat_event(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
            &stream_start("message-1"),
        )
        .await;

        assert!(replay_state.active_stream.is_none());
        assert!(
            replay_state
                .terminal_stream_message_ids
                .contains(&ChatMessageId("message-1".to_owned()))
        );
        let last = event_log.last().expect("violation is recorded");
        let event = last
            .parse_payload::<ChatEvent>()
            .expect("typed violation event");
        assert!(matches!(
            event,
            ChatEvent::MessageAdded(ChatMessage {
                sender: MessageSender::Error,
                content,
                ..
            }) if content == "Stream identity violation: duplicate terminal message id"
        ));
    }

    #[tokio::test]
    async fn replay_rejects_duplicate_same_sender_message_identity() {
        let canonical_stream = "/agent/replay-agent";
        let mut event_log = Vec::new();
        let mut replay_state = AgentReplayState::default();
        let mut subscribers = Vec::new();
        let first = ChatEvent::MessageAdded(assistant_message_with_id("message-1", "first"));
        let second = ChatEvent::MessageAdded(assistant_message_with_id("message-1", "second"));

        append_chat_event(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
            &first,
        )
        .await;
        append_chat_event(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
            &second,
        )
        .await;

        assert_eq!(
            event_log
                .iter()
                .filter(|envelope| matches!(
                    envelope.parse_payload::<ChatEvent>(),
                    Ok(ChatEvent::MessageAdded(message))
                        if message.message_id == Some(ChatMessageId("message-1".to_owned()))
                ))
                .count(),
            1
        );
        assert!(event_log.iter().any(|envelope| {
            envelope
                .payload
                .to_string()
                .contains("duplicate terminal message id")
        }));
    }

    #[tokio::test]
    async fn empty_stream_completion_remains_durable_after_idle() {
        let canonical_stream = "/agent/replay-agent";
        let mut event_log = Vec::new();
        let mut replay_state = AgentReplayState::default();
        let mut subscribers = Vec::new();

        append_chat_event(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
            &ChatEvent::TypingStatusChanged(true),
        )
        .await;
        append_chat_event(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
            &stream_start("completion-only-1"),
        )
        .await;
        append_chat_event(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
            &ChatEvent::StreamEnd(StreamEndData {
                message: assistant_message_with_id("completion-only-1", ""),
            }),
        )
        .await;
        append_chat_event(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
            &ChatEvent::TypingStatusChanged(false),
        )
        .await;

        assert!(event_log.iter().any(|envelope| {
            matches!(
                envelope.parse_payload::<ChatEvent>(),
                Ok(ChatEvent::MessageAdded(message))
                    if message.message_id == Some(ChatMessageId("completion-only-1".to_owned()))
                        && message.content.is_empty()
            )
        }));

        let (tx, mut rx) = mpsc::unbounded_channel();
        attach_subscriber(
            &event_log,
            Some(&replay_state),
            &mut subscribers,
            replay_stream(tx),
        );
        let events = recv_agent_bootstrap_events(&mut rx, "late bootstrap").await;
        assert!(events.iter().any(|event| {
            matches!(
                event,
                AgentBootstrapEvent::ChatEvent(ChatEvent::MessageAdded(message))
                    if message.message_id == Some(ChatMessageId("completion-only-1".to_owned()))
                        && message.content.is_empty()
            )
        }));
    }

    #[tokio::test]
    async fn cancelled_reasoning_stream_remains_durable_with_its_stream_identity() {
        let canonical_stream = "/agent/replay-agent";
        let mut event_log = Vec::new();
        let mut replay_state = AgentReplayState::default();
        let mut subscribers = Vec::new();

        append_chat_event(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
            &stream_start("reasoning-only-1"),
        )
        .await;
        append_chat_event(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
            &ChatEvent::StreamReasoningDelta(StreamTextDeltaData {
                message_id: Some("reasoning-only-1".to_owned()),
                text: "thinking".to_owned(),
            }),
        )
        .await;
        append_chat_event(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
            &ChatEvent::OperationCancelled(protocol::OperationCancelledData {
                message: "cancelled".to_owned(),
            }),
        )
        .await;

        assert!(event_log.iter().any(|envelope| {
            matches!(
                envelope.parse_payload::<ChatEvent>(),
                Ok(ChatEvent::MessageAdded(message))
                    if message.message_id == Some(ChatMessageId("reasoning-only-1".to_owned()))
                        && message.reasoning.as_ref().map(|reasoning| reasoning.text.as_str())
                            == Some("thinking")
            )
        }));
    }

    #[tokio::test]
    async fn live_and_replay_stream_frames_preserve_exact_message_identity() {
        let canonical_stream = "/agent/replay-agent";
        let mut event_log = Vec::new();
        let mut replay_state = AgentReplayState::default();
        let mut subscribers = Vec::new();
        let (live_tx, mut live_rx) = mpsc::unbounded_channel();
        attach_subscriber(
            &event_log,
            Some(&replay_state),
            &mut subscribers,
            replay_stream(live_tx),
        );
        let _ = recv_agent_bootstrap_events(&mut live_rx, "initial bootstrap").await;

        append_chat_event(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
            &ChatEvent::TypingStatusChanged(true),
        )
        .await;
        append_chat_event(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
            &stream_start("message-1"),
        )
        .await;
        append_chat_event(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
            &ChatEvent::StreamReasoningDelta(StreamTextDeltaData {
                message_id: Some("message-1".to_owned()),
                text: "thinking".to_owned(),
            }),
        )
        .await;
        append_chat_event(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
            &ChatEvent::StreamDelta(StreamTextDeltaData {
                message_id: Some("message-1".to_owned()),
                text: "answer".to_owned(),
            }),
        )
        .await;
        append_chat_event(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
            &ChatEvent::StreamEnd(StreamEndData {
                message: assistant_message_with_id("message-1", "answer"),
            }),
        )
        .await;

        let mut live = Vec::new();
        for _ in 0..5 {
            let envelope = timeout(Duration::from_secs(1), live_rx.recv())
                .await
                .expect("timed out waiting for live stream frame")
                .expect("live subscriber closed");
            live.push(
                envelope
                    .parse_payload::<ChatEvent>()
                    .expect("typed live chat event"),
            );
        }
        let (replay_tx, mut replay_rx) = mpsc::unbounded_channel();
        attach_subscriber(
            &event_log,
            Some(&replay_state),
            &mut subscribers,
            replay_stream(replay_tx),
        );
        let replay = recv_agent_bootstrap_events(&mut replay_rx, "late bootstrap")
            .await
            .into_iter()
            .filter_map(|event| match event {
                AgentBootstrapEvent::ChatEvent(event) => Some(event),
                _ => None,
            })
            .collect::<Vec<_>>();

        let live_stream = live
            .into_iter()
            .filter(|event| !matches!(event, ChatEvent::TypingStatusChanged(_)))
            .collect::<Vec<_>>();
        let replay_stream = replay
            .into_iter()
            .filter(|event| !matches!(event, ChatEvent::TypingStatusChanged(_)))
            .collect::<Vec<_>>();
        assert_eq!(
            serde_json::to_value(live_stream).expect("serialize live stream frames"),
            serde_json::to_value(replay_stream).expect("serialize replay stream frames")
        );
    }

    #[tokio::test]
    async fn replay_active_reasoning_preserves_stream_message_id_before_live_end() {
        let canonical_stream = "/agent/replay-agent";
        let mut event_log = Vec::new();
        let mut replay_state = AgentReplayState::default();
        let mut subscribers = Vec::new();

        append_chat_event(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
            &ChatEvent::TypingStatusChanged(true),
        )
        .await;
        append_chat_event(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
            &ChatEvent::StreamStart(StreamStartData {
                message_id: Some("reasoning-item-1".to_owned()),
                agent: "mock".to_owned(),
                model: Some("mock-model".to_owned()),
            }),
        )
        .await;
        append_chat_event(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
            &ChatEvent::StreamReasoningDelta(StreamTextDeltaData {
                message_id: Some("reasoning-item-1".to_owned()),
                text: "thinking".to_owned(),
            }),
        )
        .await;

        let (tx, mut rx) = mpsc::unbounded_channel();
        attach_subscriber(
            &event_log,
            Some(&replay_state),
            &mut subscribers,
            replay_stream(tx),
        );

        let mut events = recv_agent_bootstrap_events(&mut rx, "agent bootstrap")
            .await
            .into_iter();
        assert!(matches!(
            events.next(),
            Some(AgentBootstrapEvent::ChatEvent(
                ChatEvent::TypingStatusChanged(true)
            ))
        ));
        assert!(matches!(
            events.next(),
            Some(AgentBootstrapEvent::ChatEvent(ChatEvent::StreamStart(..)))
        ));
        match events.next() {
            Some(AgentBootstrapEvent::ChatEvent(ChatEvent::StreamReasoningDelta(delta))) => {
                assert_eq!(delta.message_id, Some("reasoning-item-1".to_owned()));
                assert_eq!(delta.text, "thinking");
            }
            other => panic!("expected aggregated StreamReasoningDelta, got {other:?}"),
        }
        assert!(events.next().is_none());

        let mut message = assistant_message("");
        message.message_id = Some(ChatMessageId("reasoning-item-1".to_owned()));
        append_chat_event(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
            &ChatEvent::StreamEnd(StreamEndData { message }),
        )
        .await;

        let env = timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("timed out waiting for live StreamEnd")
            .expect("stream closed before live StreamEnd");
        assert_eq!(env.kind, FrameKind::ChatEvent);
        match env
            .parse_payload::<ChatEvent>()
            .expect("failed to parse live StreamEnd")
        {
            ChatEvent::StreamEnd(data) => {
                assert_eq!(
                    data.message.message_id,
                    Some(ChatMessageId("reasoning-item-1".to_owned()))
                );
            }
            other => panic!("expected live StreamEnd, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn replay_preserves_active_typing_before_stream_start() {
        let canonical_stream = "/agent/replay-agent";
        let mut event_log = Vec::new();
        let mut replay_state = AgentReplayState::default();
        let mut subscribers = Vec::new();

        append_chat_event(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
            &ChatEvent::TypingStatusChanged(true),
        )
        .await;

        let (tx, mut rx) = mpsc::unbounded_channel();
        attach_subscriber(
            &event_log,
            Some(&replay_state),
            &mut subscribers,
            replay_stream(tx),
        );

        let mut events = recv_agent_bootstrap_events(&mut rx, "agent bootstrap")
            .await
            .into_iter();
        assert!(matches!(
            events.next(),
            Some(AgentBootstrapEvent::ChatEvent(
                ChatEvent::TypingStatusChanged(true)
            ))
        ));
        assert!(events.next().is_none());
    }

    #[tokio::test]
    async fn replay_preserves_completed_stream_until_idle() {
        let canonical_stream = "/agent/replay-agent";
        let mut event_log = Vec::new();
        let mut replay_state = AgentReplayState::default();
        let mut subscribers = Vec::new();

        append_chat_event(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
            &ChatEvent::TypingStatusChanged(true),
        )
        .await;
        append_chat_event(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
            &ChatEvent::StreamStart(StreamStartData {
                message_id: Some("message-1".to_owned()),
                agent: "mock".to_owned(),
                model: Some("mock-model".to_owned()),
            }),
        )
        .await;
        append_chat_event(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
            &ChatEvent::StreamDelta(StreamTextDeltaData {
                message_id: Some("message-1".to_owned()),
                text: "hello".to_owned(),
            }),
        )
        .await;
        append_chat_event(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
            &ChatEvent::StreamEnd(StreamEndData {
                message: assistant_message_with_id("message-1", "hello"),
            }),
        )
        .await;

        let (tx, mut rx) = mpsc::unbounded_channel();
        attach_subscriber(
            &event_log,
            Some(&replay_state),
            &mut subscribers,
            replay_stream(tx),
        );

        let events = recv_agent_bootstrap_events(&mut rx, "agent bootstrap").await;
        assert!(matches!(
            events.as_slice(),
            [
                AgentBootstrapEvent::ChatEvent(ChatEvent::TypingStatusChanged(true)),
                AgentBootstrapEvent::ChatEvent(ChatEvent::StreamStart(..)),
                AgentBootstrapEvent::ChatEvent(ChatEvent::StreamDelta(..)),
                AgentBootstrapEvent::ChatEvent(ChatEvent::StreamEnd(..)),
            ]
        ));
    }

    #[tokio::test]
    async fn replay_keeps_stream_tool_events_after_message() {
        let canonical_stream = "/agent/replay-agent";
        let mut event_log = Vec::new();
        let mut replay_state = AgentReplayState::default();
        let mut subscribers = Vec::new();

        append_chat_event(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
            &ChatEvent::TypingStatusChanged(true),
        )
        .await;
        append_chat_event(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
            &ChatEvent::StreamStart(StreamStartData {
                message_id: Some("message-1".to_owned()),
                agent: "mock".to_owned(),
                model: Some("mock-model".to_owned()),
            }),
        )
        .await;
        append_chat_event(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
            &ChatEvent::ToolRequest(ToolRequest {
                tool_call_id: "tool-1".to_owned(),
                tool_name: "run_command".to_owned(),
                tool_type: ToolRequestType::RunCommand {
                    command: "echo hi".to_owned(),
                    working_directory: "/tmp".to_owned(),
                },
            }),
        )
        .await;
        append_chat_event(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
            &ChatEvent::ToolExecutionCompleted(ToolExecutionCompletedData {
                tool_call_id: "tool-1".to_owned(),
                tool_name: "run_command".to_owned(),
                tool_result: ToolExecutionResult::RunCommand {
                    exit_code: 0,
                    stdout: "hi\n".to_owned(),
                    stderr: String::new(),
                },
                success: true,
                error: None,
                normalization_failure: None,
            }),
        )
        .await;
        let mut message = assistant_message_with_id("message-1", "");
        message.tool_calls = vec![ToolUseData {
            id: "tool-1".to_owned(),
            name: "run_command".to_owned(),
            arguments: serde_json::json!({
                "kind": "RunCommand",
                "command": "echo hi",
                "working_directory": "/tmp"
            }),
        }];
        append_chat_event(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
            &ChatEvent::StreamEnd(StreamEndData { message }),
        )
        .await;
        append_chat_event(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
            &ChatEvent::TypingStatusChanged(false),
        )
        .await;

        let (tx, mut rx) = mpsc::unbounded_channel();
        attach_subscriber(
            &event_log,
            Some(&replay_state),
            &mut subscribers,
            replay_stream(tx),
        );

        let mut events = recv_agent_bootstrap_events(&mut rx, "agent bootstrap")
            .await
            .into_iter();
        assert!(matches!(
            events.next(),
            Some(AgentBootstrapEvent::ChatEvent(ChatEvent::StreamStart(_)))
        ));
        assert!(matches!(
            events.next(),
            Some(AgentBootstrapEvent::ChatEvent(ChatEvent::ToolRequest(_)))
        ));
        assert!(matches!(
            events.next(),
            Some(AgentBootstrapEvent::ChatEvent(
                ChatEvent::ToolExecutionCompleted(_)
            ))
        ));
        let end = match events.next() {
            Some(AgentBootstrapEvent::ChatEvent(ChatEvent::StreamEnd(end))) => end,
            other => panic!("expected declared tool-container StreamEnd, got {other:?}"),
        };
        assert_eq!(
            end.message
                .tool_calls
                .iter()
                .map(|call| call.id.as_str())
                .collect::<Vec<_>>(),
            vec!["tool-1"]
        );
        assert!(events.next().is_none());
        assert!(
            timeout(Duration::from_millis(50), rx.recv()).await.is_err(),
            "bootstrap tail should stay inside AgentBootstrap"
        );

        let window = session_history_window(&event_log, None, 10, None);
        assert!(matches!(
            window.events.as_slice(),
            [
                ChatEvent::StreamEnd(_),
                ChatEvent::ToolExecutionCompleted(_),
                ChatEvent::ToolRequest(_),
                ChatEvent::StreamStart(_)
            ]
        ));
    }

    #[tokio::test]
    async fn failed_agent_actor_replays_terminal_error_and_rejects_input() {
        let start = AgentStartPayload {
            agent_id: protocol::AgentId("agent-failed".to_string()),
            name: "Chat".to_string(),
            origin: protocol::AgentOrigin::User,
            backend_kind: protocol::BackendKind::Tycode,
            launch_profile_id: None,
            workspace_roots: vec!["/tmp/test".to_string()],
            custom_agent_id: None,
            team_id: None,
            team_member_id: None,
            project_id: None,
            parent_agent_id: None,
            session_id: None,
            workflow: None,
            created_at_ms: 1,
        };
        let (status_handle, _rx) = AgentStatusHandle::new();
        let handle =
            spawn_failed_agent_actor(start.clone(), "backend blew up".to_string(), status_handle);
        let snapshot = handle.snapshot();
        assert_eq!(snapshot.agent_id.0, "agent-failed");
        assert_eq!(snapshot.name, "Chat");

        let (tx, mut rx) = mpsc::unbounded_channel();
        let stream = Stream::new(StreamPath("/agent/agent-failed".to_string()), tx);
        assert!(handle.attach(stream).await);
        let events = recv_agent_bootstrap_events(&mut rx, "AgentBootstrap").await;
        assert!(matches!(
            events.as_slice(),
            [AgentBootstrapEvent::AgentError(payload)]
                if payload.fatal && payload.message == "backend blew up"
        ));

        assert!(
            handle
                .send_input(AgentInput::SendMessage(protocol::SendMessagePayload {
                    message: "hello".to_string(),
                    images: None,
                    origin: None,
                    tool_response: None,
                }))
                .await,
            "terminal actors must accept mailbox input for typed rejection"
        );
        let rejection = timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("terminal rejection must be live")
            .expect("terminal subscriber remained open");
        assert_eq!(rejection.kind, FrameKind::AgentError);
        let rejection: protocol::AgentErrorPayload = rejection
            .parse_payload()
            .expect("typed live terminal rejection");
        assert_eq!(rejection.agent_id, start.agent_id);
        assert_eq!(rejection.code, protocol::AgentErrorCode::Internal);
        assert_eq!(rejection.message, "agent not running");
        assert!(!rejection.fatal);
        assert!(
            timeout(Duration::from_millis(50), rx.recv()).await.is_err(),
            "one terminal input must append exactly one live rejection"
        );
    }
}
