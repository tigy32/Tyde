use std::collections::{HashMap, HashSet, VecDeque};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use protocol::{
    AgentActivityStats, AgentActivityStatsPayload, AgentActivitySummary, AgentBootstrapEvent,
    AgentBootstrapPayload, AgentErrorCode, AgentErrorPayload, AgentId, AgentInput, AgentOrigin,
    AgentRenamedPayload, AgentStartPayload, BackendAccessMode, BackendKind, ChatEvent, ChatMessage,
    ChatMessageId, Envelope, FrameKind, MessageMetadataUpdateData, MessageOrigin, MessageSender,
    QueuedMessageEntry, QueuedMessageId, QueuedMessagesPayload, ReviewErrorContext,
    SendMessagePayload, SessionId, SessionSettingsPayload, SessionSettingsValues, SpawnCostHint,
    StreamEndData, StreamStartData, StreamTextDeltaData, TokenUsage, TokenUsageUnavailableReason,
    ToolExecutionCompletedData, ToolExecutionResult, TurnTokenUsage,
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
    Backend, BackendSession, BackendSpawnConfig, BackendStartupError, EventStream,
    StartupMcpServer, apply_session_settings_update, resolve_backend_session_settings,
    validate_session_settings_values,
};
use crate::host::HostSubAgentEmitter;
use crate::review::ReviewRegistryHandle;
use crate::store::session::SessionStore;
use crate::stream::Stream;
use crate::sub_agent::HostSubAgentSpawnTx;

pub(crate) mod customization;
pub(crate) mod registry;

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
    replay_state: &'a mut AgentReplayState,
    subscribers: &'a mut Vec<Stream>,
    queue: &'a mut VecDeque<QueuedMessageEntry>,
    rx: &'a mut mpsc::UnboundedReceiver<AgentCommand>,
}

struct AgentNameChangeContext<'a> {
    session_store: &'a Arc<Mutex<SessionStore>>,
    session_id: Option<&'a SessionId>,
    pending_alias: &'a mut Option<InitialAgentAlias>,
    current_start: &'a mut AgentStartPayload,
    start_tx: &'a watch::Sender<AgentStartPayload>,
    event_log: &'a mut [Envelope],
    subscribers: &'a mut Vec<Stream>,
}

enum AgentCommand {
    SendInput(AgentInput),
    Compact {
        summary_prompt: String,
        max_summary_bytes: usize,
        reply: oneshot::Sender<Result<CompactionSummary, String>>,
    },
    ReleaseCompaction {
        reply: oneshot::Sender<()>,
    },
    SetName {
        name: String,
        persistence: AgentNamePersistence,
        reply: oneshot::Sender<bool>,
    },
    ReadOutput {
        after_seq: Option<u64>,
        limit: usize,
        reply: oneshot::Sender<Vec<Envelope>>,
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
    typing: bool,
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

        let current_message_id = active.current_message_id.clone();
        events.push(ChatEvent::StreamStart(active.start.clone()));
        if !active.reasoning.is_empty() {
            events.push(ChatEvent::StreamReasoningDelta(StreamTextDeltaData {
                message_id: current_message_id.clone(),
                text: active.reasoning.clone(),
            }));
        }
        if !active.text.is_empty() {
            events.push(ChatEvent::StreamDelta(StreamTextDeltaData {
                message_id: current_message_id,
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
        let current_message_id = completed.stream.current_message_id.clone();
        events.push(ChatEvent::StreamStart(completed.stream.start.clone()));
        if !completed.stream.reasoning.is_empty() {
            events.push(ChatEvent::StreamReasoningDelta(StreamTextDeltaData {
                message_id: current_message_id.clone(),
                text: completed.stream.reasoning.clone(),
            }));
        }
        if !completed.stream.text.is_empty() {
            events.push(ChatEvent::StreamDelta(StreamTextDeltaData {
                message_id: current_message_id,
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
        let mut message = completed.end.message.clone();
        if message.message_id.is_none()
            && let Some(message_id) = completed.stream.current_message_id.clone().or(completed
                .stream
                .start
                .message_id
                .clone())
        {
            message.message_id = Some(ChatMessageId(message_id));
        }
        let message =
            message_has_renderable_content(&message, !tool_call_ids.is_empty()).then_some(message);
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
        if update.turn_token_usage.is_some() {
            completed.end.message.turn_token_usage = update.turn_token_usage.clone();
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
        match event {
            ChatEvent::MessageAdded(message) => self
                .message
                .as_ref()
                .is_some_and(|completed_message| same_chat_message(completed_message, message)),
            ChatEvent::ToolRequest(_)
            | ChatEvent::ToolProgress(_)
            | ChatEvent::ToolExecutionCompleted(_) => chat_event_tool_call_id(event)
                .is_some_and(|tool_call_id| self.tool_call_ids.contains(tool_call_id)),
            _ => false,
        }
    }
}

struct ReplayActiveStream {
    current_message_id: Option<String>,
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

#[derive(Clone, Copy)]
enum AgentNamePersistence {
    User,
    GeneratedIfNoUserAlias,
}

#[derive(Clone)]
pub(crate) struct AgentHandle {
    tx: mpsc::UnboundedSender<AgentCommand>,
    accepting_input: Arc<AtomicBool>,
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
}

#[derive(Debug, Default)]
struct AgentActivityStatsTracker {
    stats: AgentActivityStats,
    seen_tool_calls: HashSet<String>,
    token_usage_by_source: HashMap<TokenUsageSource, TokenUsage>,
    active_reasoning: String,
}

impl AgentActivityStatsTracker {
    fn snapshot(&self) -> AgentActivityStats {
        self.stats.clone()
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
                    self.update_last_output(&message.content, source_seq);
                    self.stamp_message_turn_token_usage(message, source_seq);
                }
            }
            ChatEvent::MessageMetadataUpdated(update) => {
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
            | ChatEvent::RetryAttempt(_) => {}
            ChatEvent::StreamStart(_) => {
                self.active_reasoning.clear();
            }
        }
        self.stats != previous
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
        let source = token_usage_source_for_message(message, source_seq);
        message.turn_token_usage =
            Some(self.turn_token_usage_for_source(source, message.token_usage.clone(), source_seq));
    }

    fn stamp_metadata_turn_token_usage(
        &mut self,
        update: &mut MessageMetadataUpdateData,
        source_seq: u64,
    ) {
        if update.token_usage.is_none() {
            return;
        }
        update.turn_token_usage = Some(self.turn_token_usage_for_source(
            TokenUsageSource::Message(update.message_id.clone()),
            update.token_usage.clone(),
            source_seq,
        ));
    }

    fn turn_token_usage_for_source(
        &mut self,
        source: TokenUsageSource,
        token_usage: Option<TokenUsage>,
        source_seq: u64,
    ) -> TurnTokenUsage {
        let Some(token_usage) = token_usage else {
            return TurnTokenUsage::Unavailable {
                reason: TokenUsageUnavailableReason::BackendDidNotReport,
            };
        };
        self.token_usage_by_source
            .insert(source, token_usage.clone());
        self.refresh_token_usage();
        self.stats.source_through_seq = Some(source_seq);
        TurnTokenUsage::Known {
            this_turn: Box::new(token_usage),
            agent_total: Box::new(self.stats.token_usage.clone()),
        }
    }

    fn refresh_token_usage(&mut self) {
        self.stats.token_usage = total_token_usage(self.token_usage_by_source.values());
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
        if !self.accepting_input.load(Ordering::SeqCst) {
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
        let (reply_tx, reply_rx) = oneshot::channel();
        if self
            .tx
            .send(AgentCommand::SetName {
                name,
                persistence: AgentNamePersistence::User,
                reply: reply_tx,
            })
            .is_err()
        {
            return None;
        }
        reply_rx.await.ok()
    }

    pub async fn set_generated_name(&self, name: String) -> Option<bool> {
        let (reply_tx, reply_rx) = oneshot::channel();
        if self
            .tx
            .send(AgentCommand::SetName {
                name,
                persistence: AgentNamePersistence::GeneratedIfNoUserAlias,
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

    pub async fn attach(&self, stream: Stream) -> bool {
        let (reply_tx, reply_rx) = oneshot::channel();
        if self
            .tx
            .send(AgentCommand::Attach {
                stream,
                reply: reply_tx,
            })
            .is_err()
        {
            return false;
        }
        reply_rx.await.unwrap_or(false)
    }
}

enum ActorLifecycle {
    Running,
    Closing,
}

pub(crate) struct GenerateAgentNameRequest {
    pub backend_kind: BackendKind,
    pub workspace_roots: Vec<String>,
    pub prompt: String,
    pub startup_mcp_servers: Vec<StartupMcpServer>,
    pub use_mock_backend: bool,
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
}

pub(crate) async fn generate_agent_name(
    request: GenerateAgentNameRequest,
) -> Result<String, String> {
    let prompt = request.prompt.trim();
    if prompt.is_empty() {
        return Ok(IMAGE_ONLY_AGENT_NAME.to_string());
    }

    if request.use_mock_backend {
        return generate_mock_name(prompt);
    }

    let name_prompt = build_name_generation_prompt(prompt);
    let logged_name_prompt = name_prompt.clone();
    let startup_mcp_server_names = request
        .startup_mcp_servers
        .iter()
        .map(|server| server.name.clone())
        .collect::<Vec<_>>();
    let spawn_config = BackendSpawnConfig {
        cost_hint: Some(SpawnCostHint::Low),
        custom_agent_id: None,
        startup_mcp_servers: request.startup_mcp_servers,
        session_settings: None,
        backend_config: Default::default(),
        resolved_spawn_config: Default::default(),
    };
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
        request.workspace_roots,
        spawn_config,
        initial_input,
        host_sub_agent_spawn_tx,
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

    let mut streamed_text = String::new();
    let mut stream_delta_count = 0usize;
    let mut chat_event_count = 0usize;
    while let Some(event) = events.recv().await {
        chat_event_count += 1;
        match event {
            ChatEvent::MessageAdded(message) if matches!(message.sender, MessageSender::Error) => {
                tracing::warn!(
                    backend_kind = ?request.backend_kind,
                    cost_hint = ?SpawnCostHint::Low,
                    prompt = %prompt,
                    name_prompt = %logged_name_prompt,
                    chat_event_count,
                    stream_delta_count,
                    startup_mcp_servers = ?startup_mcp_server_names,
                    error_message = %message.content,
                    "agent name generator received a backend error"
                );
                return Err(message.content);
            }
            ChatEvent::StreamDelta(delta) => {
                stream_delta_count += 1;
                streamed_text.push_str(&delta.text);
            }
            ChatEvent::StreamEnd(data) => {
                let final_content = data.message.content;
                let streamed_text_len = streamed_text.len();
                let candidate = if final_content.trim().is_empty() {
                    streamed_text
                } else {
                    final_content.clone()
                };
                if candidate.trim().is_empty() {
                    tracing::warn!(
                        backend_kind = ?request.backend_kind,
                        cost_hint = ?SpawnCostHint::Low,
                        prompt = %prompt,
                        name_prompt = %logged_name_prompt,
                        chat_event_count,
                        stream_delta_count,
                        final_content_len = final_content.len(),
                        streamed_text_len,
                        startup_mcp_servers = ?startup_mcp_server_names,
                        "agent name generator received an empty assistant response"
                    );
                }
                return Ok(name_generation_fallback(prompt, &candidate));
            }
            _ => {}
        }
    }

    tracing::warn!(
        backend_kind = ?request.backend_kind,
        cost_hint = ?SpawnCostHint::Low,
        prompt = %prompt,
        name_prompt = %logged_name_prompt,
        chat_event_count,
        stream_delta_count,
        startup_mcp_servers = ?startup_mcp_server_names,
        "agent name generator ended before producing a final response"
    );
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
        host_sub_agent_spawn_tx,
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

    let mut streamed_text = String::new();
    let mut stream_delta_count = 0usize;
    let mut chat_event_count = 0usize;
    let mut backend_error: Option<String> = None;
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
                let candidate = if final_content.trim().is_empty() {
                    streamed_text
                } else {
                    final_content
                };
                let text = sanitize_activity_summary_text(&candidate)?;
                return Ok(AgentActivitySummary {
                    text,
                    generated_at_ms: now_ms(),
                    source_from_seq: request.source_from_seq,
                    source_through_seq: request.source_through_seq,
                });
            }
            ChatEvent::ToolRequest(requested_tool) => {
                tracing::warn!(
                    summary_agent_id = %request.summary_agent_id,
                    backend_kind = ?request.backend_kind,
                    tool_name = %requested_tool.tool_name,
                    "activity summary generator attempted a tool call; ignoring and continuing to read text"
                );
            }
            _ => {}
        }
    }

    if !streamed_text.trim().is_empty() {
        let text = sanitize_activity_summary_text(&streamed_text)?;
        return Ok(AgentActivitySummary {
            text,
            generated_at_ms: now_ms(),
            source_from_seq: request.source_from_seq,
            source_through_seq: request.source_through_seq,
        });
    }

    tracing::warn!(
        summary_agent_id = %request.summary_agent_id,
        backend_kind = ?request.backend_kind,
        cost_hint = ?SpawnCostHint::Low,
        prompt_len = logged_prompt_len,
        target_workspace_root_count,
        chat_event_count,
        stream_delta_count,
        "agent activity summary generator ended without usable assistant text"
    );
    Err(backend_error.unwrap_or_else(|| {
        "agent activity summary generator ended before producing usable text".to_owned()
    }))
}

/// Type-erased backend handle. The actor loop only needs `send()` — this lets
/// us dispatch to any concrete `Backend` at spawn time and forget the type.
trait BackendSender: Send + 'static {
    fn send<'a>(
        &'a self,
        input: AgentInput,
    ) -> Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>>;
    fn interrupt<'a>(&'a self) -> Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>>;
    fn shutdown(self: Box<Self>) -> Pin<Box<dyn std::future::Future<Output = ()> + Send>>;
}

impl<B: Backend> BackendSender for B {
    fn send<'a>(
        &'a self,
        input: AgentInput,
    ) -> Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>> {
        Box::pin(Backend::send(self, input))
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
    host_sub_agent_spawn_tx: HostSubAgentSpawnTx,
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
            let emitter = Arc::new(HostSubAgentEmitter::new(
                host_sub_agent_spawn_tx,
                agent_id.clone(),
                workspace_roots.clone(),
            ));
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
            let (b, events) =
                CodexBackend::spawn(workspace_roots.clone(), config, initial_input).await?;
            let session_id = Backend::session_id(&b);
            b.set_subagent_emitter(Arc::new(HostSubAgentEmitter::new(
                host_sub_agent_spawn_tx,
                agent_id.clone(),
                workspace_roots,
            )))
            .await;
            Ok((Box::new(b), events, session_id))
        }
        BackendKind::Antigravity => {
            let (b, events) =
                AntigravityBackend::spawn(workspace_roots, config, initial_input).await?;
            let session_id = Backend::session_id(&b);
            Ok((Box::new(b), events, session_id))
        }
        BackendKind::Hermes => {
            let (b, events) = HermesBackend::spawn(workspace_roots, config, initial_input).await?;
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
    host_sub_agent_spawn_tx: HostSubAgentSpawnTx,
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
            b.set_subagent_emitter(Arc::new(HostSubAgentEmitter::new(
                host_sub_agent_spawn_tx,
                agent_id.clone(),
                workspace_roots,
            )))
            .await;
            (Box::new(b), events)
        }
        BackendKind::Codex => {
            let (b, events) =
                CodexBackend::resume(workspace_roots.clone(), config, session_id.clone()).await?;
            b.set_subagent_emitter(Arc::new(HostSubAgentEmitter::new(
                host_sub_agent_spawn_tx,
                agent_id.clone(),
                workspace_roots,
            )))
            .await;
            (Box::new(b), events)
        }
        BackendKind::Antigravity => {
            let (b, events) =
                AntigravityBackend::resume(workspace_roots, config, session_id).await?;
            (Box::new(b), events)
        }
        BackendKind::Hermes => {
            let (b, events) = HermesBackend::resume(workspace_roots, config, session_id).await?;
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
    host_sub_agent_spawn_tx: HostSubAgentSpawnTx,
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
            b.set_subagent_emitter(Arc::new(HostSubAgentEmitter::new(
                host_sub_agent_spawn_tx,
                agent_id.clone(),
                workspace_roots,
            )))
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
            b.set_subagent_emitter(Arc::new(HostSubAgentEmitter::new(
                host_sub_agent_spawn_tx,
                agent_id.clone(),
                workspace_roots,
            )))
            .await;
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
    host_sub_agent_spawn_tx: HostSubAgentSpawnTx,
) -> BackendFuture<BackendSpawnResult> {
    Box::pin(async move {
        let (b, events) =
            MockBackend::spawn(workspace_roots.clone(), config, initial_input).await?;
        let sid = Backend::session_id(&b);
        b.set_subagent_emitter(Arc::new(HostSubAgentEmitter::new(
            host_sub_agent_spawn_tx,
            agent_id,
            workspace_roots,
        )))
        .await;
        Ok((Box::new(b) as BackendHandle, events, sid))
    })
}

fn resume_mock(
    agent_id: AgentId,
    workspace_roots: Vec<String>,
    session_id: SessionId,
    host_sub_agent_spawn_tx: HostSubAgentSpawnTx,
) -> BackendFuture<BackendResumeResult> {
    Box::pin(async move {
        let (b, events) = MockBackend::resume(
            workspace_roots.clone(),
            BackendSpawnConfig::default(),
            session_id.clone(),
        )
        .await?;
        b.set_subagent_emitter(Arc::new(HostSubAgentEmitter::new(
            host_sub_agent_spawn_tx,
            agent_id,
            workspace_roots,
        )))
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
    host_sub_agent_spawn_tx: HostSubAgentSpawnTx,
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
        b.set_subagent_emitter(Arc::new(HostSubAgentEmitter::new(
            host_sub_agent_spawn_tx,
            agent_id,
            workspace_roots,
        )))
        .await;
        Ok((Box::new(b) as BackendHandle, events, sid))
    })
}

pub(crate) fn spawn_agent_actor(
    agent_id: AgentId,
    start: AgentStartPayload,
    request: ResolvedSpawnRequest,
    session_store: Arc<Mutex<SessionStore>>,
    host_sub_agent_spawn_tx: HostSubAgentSpawnTx,
    review_registry: ReviewRegistryHandle,
    status_handle: registry::AgentStatusHandle,
) -> (AgentHandle, oneshot::Receiver<Result<SessionId, String>>) {
    let (tx, mut rx) = mpsc::unbounded_channel::<AgentCommand>();
    let accepting_input = Arc::new(AtomicBool::new(false));
    let accepting_input_task = Arc::clone(&accepting_input);
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
            workflow: _,
            ..
        } = request;
        let mut current_start = start.clone();
        let spawn_config = BackendSpawnConfig {
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
        let mut replay_state = AgentReplayState::default();
        let mut subscribers: Vec<Stream> = Vec::new();
        let mut active_stream_text = String::new();
        let mut activity_stats = AgentActivityStatsTracker::default();
        let mut activity_event_seq = 0_u64;
        let mut current_session_id = resume_session_id.clone();
        let mut pending_alias = initial_alias;
        let session_schema = session_settings_schema;
        let mut current_session_settings = resolve_backend_session_settings(
            backend_kind,
            &BackendSpawnConfig {
                cost_hint: initial_cost_hint,
                custom_agent_id: current_start.custom_agent_id.clone(),
                startup_mcp_servers: Vec::new(),
                session_settings: initial_session_settings,
                backend_config,
                resolved_spawn_config,
            },
        );
        let mut queue = VecDeque::new();
        assert!(
            resume_session_id.is_none() || fork_from_session_id.is_none(),
            "spawn request cannot both resume and fork a session"
        );
        let starts_with_initial_turn = resume_session_id.is_none();
        let is_resume = resume_session_id.is_some();

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
                            host_sub_agent_spawn_tx.clone(),
                        )
                        .await
                    } else {
                        resume_backend(
                            &agent_id,
                            backend_kind,
                            workspace_roots.clone(),
                            spawn_config.clone(),
                            session_id.clone(),
                            host_sub_agent_spawn_tx.clone(),
                        )
                        .await
                    };
                    resumed
                        .map(|(backend, events)| (backend, events, session_id, initial_input))
                        .map_err(AgentStartupFailure::backend_failed)
                }
                None => {
                    if let Some(from_session_id) = fork_from_session_id {
                        let first_input = initial_input.expect("fork spawn requires initial_input");
                        let forked = if use_mock_backend {
                            fork_mock(
                                agent_id.clone(),
                                workspace_roots.clone(),
                                spawn_config,
                                from_session_id,
                                first_input,
                                host_sub_agent_spawn_tx.clone(),
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
                                host_sub_agent_spawn_tx.clone(),
                            )
                            .await
                        };
                        forked
                            .map(|(backend, events, session_id)| {
                                (backend, events, session_id, None)
                            })
                            .map_err(AgentStartupFailure::from)
                    } else {
                        let first_input = initial_input.expect("new spawn requires initial_input");
                        let spawned = if use_mock_backend {
                            spawn_mock(
                                agent_id.clone(),
                                workspace_roots.clone(),
                                spawn_config,
                                first_input,
                                host_sub_agent_spawn_tx.clone(),
                            )
                            .await
                        } else {
                            spawn_backend(
                                &agent_id,
                                backend_kind,
                                workspace_roots.clone(),
                                spawn_config,
                                first_input,
                                host_sub_agent_spawn_tx.clone(),
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
                park_terminal_agent(
                    &session_store,
                    current_session_id.as_ref(),
                    &mut pending_alias,
                    &mut current_start,
                    &start_tx,
                    &mut event_log,
                    &mut subscribers,
                    &mut rx,
                )
                .await;
                return;
            }
        };
        let mut backend = Some(backend);
        let mut in_turn = starts_with_initial_turn;
        let mut idle_transition_armed = false;
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
        let _ = startup_tx.send(Ok(actor_session_id.clone()));
        accepting_input_task.store(!resume_replay_gate_pending, Ordering::SeqCst);
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
                    replay_state: &mut replay_state,
                    subscribers: &mut subscribers,
                    queue: &mut queue,
                    rx: &mut rx,
                },
            )
            .await
        {
            abort_resume_replay_barrier_task(&mut resume_replay_barrier_task);
            return;
        }

        loop {
            tokio::select! {
                maybe_event = events.recv() => {
                    let Some(mut event) = maybe_event else {
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
                            flush_pending_resume_attaches(
                                &event_log,
                                None,
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
                                &mut subscribers,
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
                            &mut subscribers,
                            &mut rx,
                        )
                        .await;
                        return;
                    };
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
                                let error_ends_turn =
                                    in_turn && pending_tool_response_ids.is_empty();
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
                                    s.turn_completed = true;
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
                            if typing {
                                in_turn = true;
                                idle_transition_armed = true;
                            } else if !pending_tool_response_ids.is_empty() {
                                idle_transition_armed = false;
                            } else if in_turn && idle_transition_armed {
                                real_idle_transition = true;
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
                            Some(MessageOrigin::User) | None => None,
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
                        let sent = backend
                            .as_ref()
                            .expect("backend must exist while actor is running")
                            .send(AgentInput::SendMessage(queued_message_to_send_payload(
                                queued.clone(),
                            )))
                            .await;
                        if !sent {
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
                                &mut subscribers,
                                &mut rx,
                            )
                            .await;
                            return;
                        }
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
                maybe_command = rx.recv() => {
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
                            while let Ok(mut event) = events.try_recv() {
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
                                    flush_pending_resume_attaches(
                                        &event_log,
                                        Some(&replay_state),
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
                                                replay_state: &mut replay_state,
                                                subscribers: &mut subscribers,
                                                queue: &mut queue,
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
                                    flush_pending_resume_attaches(
                                        &event_log,
                                        None,
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
                                        &mut subscribers,
                                        &mut rx,
                                    )
                                    .await;
                                    return;
                                }
                            }
                        }
                        AgentCommand::SendInput(input) => {
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
                                        Some(MessageOrigin::User) | None => None,
                                    };
                                    let message_len = msg.message.len();
                                    let images_count = msg.images.as_ref().map_or(0, Vec::len);
                                    let review_origin_for_queue = match msg.origin.clone() {
                                        Some(MessageOrigin::Review { review_id }) => Some(review_id),
                                        Some(MessageOrigin::User) | None => None,
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
                                        let sent = backend
                                            .as_ref()
                                            .expect("backend must exist while actor is running")
                                            .send(AgentInput::SendMessage(msg))
                                            .await;
                                        if !sent {
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
                                                &mut subscribers,
                                                &mut rx,
                                            )
                                            .await;
                                            return;
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
                                        Some(MessageOrigin::User) | None => None,
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
                                    let sent = backend
                                        .as_ref()
                                        .expect("backend must exist while actor is running")
                                        .send(AgentInput::SendMessage(
                                            queued_message_to_send_payload(queued.clone()),
                                        ))
                                        .await;
                                    if !sent {
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
                                            &mut subscribers,
                                            &mut rx,
                                        )
                                        .await;
                                        return;
                                    }
                                    if let Some(review_id) = review_origin.as_ref() {
                                        tracing::info!(
                                            review_id = %review_id,
                                            agent_id = %current_start.agent_id,
                                            queued_message_id = %queued.id,
                                            "sent immediate review-origin bundle to backend"
                                        );
                                    }
                                    if let Some(MessageOrigin::Review { review_id }) = queued.origin
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
                                    if let Err(err) =
                                        validate_session_settings_values(session_schema, &update.values)
                                    {
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
                                    apply_session_settings_update(
                                        &mut current_session_settings,
                                        &update.values,
                                    );
                                    let _ = backend
                                        .as_ref()
                                        .expect("backend must exist while actor is running")
                                        .send(AgentInput::UpdateSessionSettings(update))
                                        .await;
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
                            if !backend
                                .as_ref()
                                .expect("backend must exist while actor is running")
                                .send(AgentInput::SendMessage(SendMessagePayload {
                                    message: summary_prompt,
                                    images: None,
                                    origin: None,
                                    tool_response: None,
                                }))
                                .await
                            {
                                let compaction = active_compaction
                                    .take()
                                    .expect("active compaction disappeared after backend send failed");
                                compaction_blocked = false;
                                in_turn = false;
                                idle_transition_armed = false;
                                status_handle
                                    .update(|s| {
                                        s.is_thinking = false;
                                        s.turn_completed = true;
                                        s.last_error = Some("agent backend closed".to_owned());
                                        s.activity_counter = s.activity_counter.saturating_add(1);
                                    })
                                    .await;
                                let _ = compaction
                                    .reply
                                    .send(Err("agent backend closed".to_owned()));
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
                        AgentCommand::ReadOutput {
                            after_seq,
                            limit,
                            reply,
                        } => {
                            let _ = reply.send(output_events_since(&event_log, after_seq, limit));
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
                            let attached = attach_subscriber(
                                &event_log,
                                Some(&replay_state),
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
            start: start_rx,
        },
        startup_rx,
    )
}

pub(crate) fn spawn_relay_agent_actor(
    agent_id: AgentId,
    start: AgentStartPayload,
    mut events: mpsc::UnboundedReceiver<ChatEvent>,
    session_store: Arc<Mutex<SessionStore>>,
    session_id: SessionId,
    status_handle: registry::AgentStatusHandle,
) -> AgentHandle {
    let (tx, mut rx) = mpsc::unbounded_channel::<AgentCommand>();
    let accepting_input = Arc::new(AtomicBool::new(true));
    let accepting_input_task = Arc::clone(&accepting_input);
    let (start_tx, start_rx) = watch::channel(start.clone());

    tokio::spawn(async move {
        let canonical_stream = format!("/agent/{}", agent_id);
        let mut event_log: Vec<Envelope> = Vec::new();
        let mut replay_state = AgentReplayState::default();
        let mut subscribers: Vec<Stream> = Vec::new();
        let mut active_stream_text = String::new();
        let mut activity_stats = AgentActivityStatsTracker::default();
        let mut activity_event_seq = 0_u64;
        let mut current_start = start;
        let mut pending_alias = None;
        let mut in_turn = false;
        let mut pending_tool_response_ids: HashSet<String> = HashSet::new();
        let mut lifecycle = ActorLifecycle::Running;
        let mut close_reply: Option<oneshot::Sender<()>> = None;

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
            tokio::select! {
                maybe_event = events.recv() => {
                    let Some(mut event) = maybe_event else {
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
                                s.activity_counter = s.activity_counter.saturating_add(1);
                            }).await;
                        }
                        ChatEvent::OperationCancelled(_) => {
                            pending_tool_response_ids.clear();
                            status_handle.update(|s| {
                                s.pending_user_response = None;
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
                        AgentCommand::ReadOutput {
                            after_seq,
                            limit,
                            reply,
                        } => {
                            let _ = reply.send(output_events_since(&event_log, after_seq, limit));
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
                            let attached = attach_subscriber(
                                &event_log,
                                Some(&replay_state),
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

#[allow(clippy::too_many_arguments)]
async fn park_terminal_agent(
    session_store: &Arc<Mutex<SessionStore>>,
    session_id: Option<&SessionId>,
    pending_alias: &mut Option<InitialAgentAlias>,
    current_start: &mut AgentStartPayload,
    start_tx: &watch::Sender<AgentStartPayload>,
    event_log: &mut [Envelope],
    subscribers: &mut Vec<Stream>,
    rx: &mut mpsc::UnboundedReceiver<AgentCommand>,
) {
    loop {
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
            AgentCommand::ReadOutput {
                after_seq,
                limit,
                reply,
            } => {
                let _ = reply.send(output_events_since(event_log, after_seq, limit));
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
            AgentCommand::Attach { stream, reply } => {
                let attached = attach_subscriber(event_log, None, subscribers, stream);
                let _ = reply.send(attached);
            }
            AgentCommand::Close { reply } => {
                let _ = reply.send(());
                break;
            }
            AgentCommand::Compact { reply, .. } => {
                let _ = reply.send(Err("agent is not running".to_owned()));
            }
            AgentCommand::ReleaseCompaction { reply } => {
                let _ = reply.send(());
            }
            AgentCommand::SendInput(_) => {}
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
    subscribers: &mut Vec<Stream>,
    rx: &mut mpsc::UnboundedReceiver<AgentCommand>,
    accepting_input: &Arc<AtomicBool>,
    status_handle: &registry::AgentStatusHandle,
    canonical_stream: &str,
) {
    loop {
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
            AgentCommand::ReadOutput {
                after_seq,
                limit,
                reply,
            } => {
                let _ = reply.send(output_events_since(event_log, after_seq, limit));
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
            AgentCommand::Attach { stream, reply } => {
                let attached = attach_subscriber(event_log, None, subscribers, stream);
                let _ = reply.send(attached);
            }
            AgentCommand::Close { reply } => {
                finish_actor_close(accepting_input, status_handle, reply).await;
                break;
            }
            AgentCommand::Compact { reply, .. } => {
                let _ = reply.send(Err("backend-native agents cannot be compacted".to_owned()));
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

async fn apply_agent_name_change(
    context: AgentNameChangeContext<'_>,
    name: String,
    persistence: AgentNamePersistence,
) -> bool {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return false;
    }

    let persisted = if let Some(session_id) = context.session_id {
        let persist_result = {
            let store = context.session_store.lock().await;
            match persistence {
                AgentNamePersistence::User => store
                    .set_user_alias(session_id, trimmed.to_string())
                    .map(|_| true),
                AgentNamePersistence::GeneratedIfNoUserAlias => {
                    store.set_generated_alias_if_no_user_alias(session_id, trimmed.to_string())
                }
            }
        };
        match persist_result {
            Ok(persisted) => persisted,
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
        match persistence {
            AgentNamePersistence::User => {
                *context.pending_alias = Some(InitialAgentAlias {
                    name: trimmed.to_string(),
                    persistence: InitialAgentAliasPersistence::User,
                });
                true
            }
            AgentNamePersistence::GeneratedIfNoUserAlias => {
                if context.pending_alias.as_ref().is_some_and(|alias| {
                    matches!(alias.persistence, InitialAgentAliasPersistence::User)
                }) {
                    false
                } else {
                    *context.pending_alias = Some(InitialAgentAlias {
                        name: trimmed.to_string(),
                        persistence: InitialAgentAliasPersistence::GeneratedIfNoUserAlias,
                    });
                    true
                }
            }
        }
    };
    if !persisted {
        return false;
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
    record_chat_event_for_replay(canonical_stream, event_log, replay_state, event);
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

fn flush_pending_resume_attaches(
    event_log: &[Envelope],
    replay_state: Option<&AgentReplayState>,
    subscribers: &mut Vec<Stream>,
    pending_attaches: &mut Vec<(Stream, oneshot::Sender<bool>)>,
) {
    for (stream, reply) in std::mem::take(pending_attaches) {
        let attached = attach_subscriber(event_log, replay_state, subscribers, stream);
        let _ = reply.send(attached);
    }
}

async fn send_initial_follow_up_or_park(
    input: SendMessagePayload,
    context: InitialFollowUpContext<'_>,
) -> bool {
    *context.in_turn = true;
    *context.idle_transition_armed = false;
    if context
        .backend
        .as_ref()
        .expect("backend must exist after successful startup")
        .send(AgentInput::SendMessage(input))
        .await
    {
        return true;
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
        context.subscribers,
        context.rx,
    )
    .await;
    false
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
            replay_state.completed_stream = None;
            replay_state.active_stream = Some(ReplayActiveStream {
                current_message_id: start.message_id.clone(),
                start: start.clone(),
                text: String::new(),
                reasoning: String::new(),
                tool_events: Vec::new(),
            });
        }
        ChatEvent::StreamDelta(delta) => {
            if let Some(active) = replay_state.active_stream.as_mut() {
                if let Some(message_id) = &delta.message_id {
                    active.current_message_id = Some(message_id.clone());
                }
                active.text.push_str(&delta.text);
            } else {
                tracing::error!(
                    "received StreamDelta without active stream while recording replay"
                );
                push_chat_event_to_replay_log(canonical_stream, event_log, event);
            }
        }
        ChatEvent::StreamReasoningDelta(delta) => {
            if let Some(active) = replay_state.active_stream.as_mut() {
                if let Some(message_id) = &delta.message_id {
                    active.current_message_id = Some(message_id.clone());
                }
                active.reasoning.push_str(&delta.text);
            } else {
                tracing::error!(
                    "received StreamReasoningDelta without active stream while recording replay"
                );
                push_chat_event_to_replay_log(canonical_stream, event_log, event);
            }
        }
        ChatEvent::StreamEnd(data) => {
            let active_stream = replay_state.active_stream.take();
            let mut message = data.message.clone();
            let tool_events = active_stream
                .map(|stream| {
                    if message.message_id.is_none()
                        && let Some(message_id) = stream
                            .current_message_id
                            .clone()
                            .or(stream.start.message_id.clone())
                    {
                        message.message_id = Some(ChatMessageId(message_id));
                    }
                    let tool_events = stream.tool_events.clone();
                    let end = StreamEndData {
                        message: message.clone(),
                    };
                    replay_state.completed_stream = Some(ReplayCompletedStream {
                        stream,
                        end,
                        post_end_events: Vec::new(),
                    });
                    tool_events
                })
                .unwrap_or_default();
            if message_has_renderable_content(&message, !tool_events.is_empty()) {
                push_chat_event_to_replay_log(
                    canonical_stream,
                    event_log,
                    &ChatEvent::MessageAdded(message),
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
        }
        ChatEvent::MessageMetadataUpdated(update) => {
            replay_state.update_completed_stream_metadata(update);
            push_chat_event_to_replay_log(
                canonical_stream,
                event_log,
                &ChatEvent::MessageMetadataUpdated(update.clone()),
            );
        }
        ChatEvent::ToolRequest(_) | ChatEvent::ToolExecutionCompleted(_) => {
            if let Some(active) = replay_state.active_stream.as_mut() {
                active.tool_events.push(event.clone());
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
            replay_state.clear_active_stream();
            push_chat_event_to_replay_log(canonical_stream, event_log, event);
        }
        ChatEvent::TypingStatusChanged(typing) => {
            replay_state.typing = *typing;
            if !typing {
                replay_state.completed_stream = None;
            }
            push_chat_event_to_replay_log(canonical_stream, event_log, event);
        }
        ChatEvent::MessageAdded(_) | ChatEvent::TaskUpdate(_) | ChatEvent::RetryAttempt(_) => {
            push_chat_event_to_replay_log(canonical_stream, event_log, event);
        }
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

fn attach_subscriber(
    event_log: &[Envelope],
    replay_state: Option<&AgentReplayState>,
    subscribers: &mut Vec<Stream>,
    stream: Stream,
) -> bool {
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
    }

    let payload = serde_json::to_value(AgentBootstrapPayload { events })
        .expect("failed to serialize AgentBootstrap payload");
    if stream
        .send_value(FrameKind::AgentBootstrap, payload)
        .is_err()
    {
        return false;
    }

    subscribers.push(stream);
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
        .filter(|(seq, event)| *seq < before_seq && matches!(event, ChatEvent::MessageAdded(_)))
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
        .filter(|(_, event)| matches!(event, ChatEvent::MessageAdded(_)))
        .count();
    if message_count <= limit {
        return 0;
    }

    let messages_to_skip = message_count - limit;
    let mut skipped = 0;
    entries[..end]
        .iter()
        .position(|(_, event)| {
            if !matches!(event, ChatEvent::MessageAdded(_)) {
                return false;
            }
            if skipped == messages_to_skip {
                return true;
            }
            skipped += 1;
            false
        })
        .expect("message_count > limit requires a history window start message")
}

fn session_history_entries_from_log(event_log: &[Envelope]) -> Vec<(u64, ChatEvent)> {
    let mut events = Vec::new();
    for envelope in event_log {
        if envelope.kind != FrameKind::ChatEvent {
            continue;
        }
        let event: ChatEvent = serde_json::from_value(envelope.payload.clone())
            .expect("failed to parse ChatEvent from replay log");
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
        let ChatEvent::MessageAdded(message) = &mut event.1 else {
            continue;
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
        if update.turn_token_usage.is_some() {
            message.turn_token_usage = update.turn_token_usage.clone();
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
                if let Some(delta) = data
                    .message
                    .token_usage
                    .as_ref()
                    .map(|usage| usage.total_tokens)
                {
                    record.token_count =
                        Some(record.token_count.unwrap_or(0).saturating_add(delta));
                }
            }),
            ChatEvent::MessageMetadataUpdated(data) => store.update(session_id, |record| {
                record.updated_at_ms = now_ms();
                if let Some(delta) = data.token_usage.as_ref().map(|usage| usage.total_tokens) {
                    record.token_count =
                        Some(record.token_count.unwrap_or(0).saturating_add(delta));
                }
            }),
            ChatEvent::TaskUpdate(tasks) => {
                let title = tasks.title.trim();
                store.update(session_id, |record| {
                    record.updated_at_ms = now_ms();
                    if !title.is_empty() && record.alias.is_none() {
                        record.alias = Some(title.to_string());
                    }
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

fn build_name_generation_prompt(prompt: &str) -> String {
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

fn name_generation_fallback(prompt: &str, generated: &str) -> String {
    match sanitize_generated_agent_name(generated) {
        Ok(name) => name,
        Err(err) => {
            tracing::warn!(
                "agent name generator produced invalid output {:?}: {}; falling back to prompt-derived name",
                generated,
                err
            );
            derive_agent_name(prompt)
        }
    }
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

    if words.len() < 2 || words.len() > 4 {
        return Err(format!(
            "generated agent name must contain 2-4 words, got {:?}",
            stripped
        ));
    }

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
        AgentId, AgentInput, AgentStartPayload, ChatEvent, ChatMessage, ChatMessageId, FrameKind,
        MessageMetadataUpdateData, MessageSender, ModelInfo, SessionId, StreamEndData, StreamPath,
        StreamStartData, StreamTextDeltaData, TaskList, TokenUsage, ToolExecutionCompletedData,
        ToolExecutionResult, ToolRequest, ToolRequestType,
    };
    use tokio::sync::{Mutex, mpsc, watch};
    use tokio::time::timeout;

    use super::{
        AgentActivityStatsTracker, AgentCommand, AgentHandle, AgentReplayState, InterruptOutcome,
        activity_history_snapshot, append_chat_event, append_event, attach_subscriber,
        generate_mock_name, name_generation_fallback, output_events_since,
        sanitize_generated_agent_name, session_history_window, spawn_relay_agent_actor,
        upsert_activity_stats_snapshot,
    };
    use crate::agent::registry::AgentStatusHandle;
    use crate::store::session::SessionStore;
    use crate::stream::Stream;

    fn spawn_failed_agent_actor(
        start: AgentStartPayload,
        error: String,
        status_handle: AgentStatusHandle,
    ) -> AgentHandle {
        let (tx, mut rx) = mpsc::unbounded_channel::<AgentCommand>();
        let accepting_input = Arc::new(AtomicBool::new(false));
        let accepting_input_task = Arc::clone(&accepting_input);
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
            let mut subscribers = Vec::new();
            append_event(
                &format!("/agent/{}", start.agent_id),
                &mut event_log,
                &mut subscribers,
                FrameKind::AgentError,
                &payload,
            )
            .await;

            while let Some(command) = rx.recv().await {
                match command {
                    AgentCommand::ResumeReplayBarrier { .. } => {}
                    AgentCommand::ReadOutput {
                        after_seq,
                        limit,
                        reply,
                    } => {
                        let _ = reply.send(output_events_since(&event_log, after_seq, limit));
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
                    AgentCommand::Attach { stream, reply } => {
                        let attached =
                            attach_subscriber(&event_log, None, &mut subscribers, stream);
                        let _ = reply.send(attached);
                    }
                    AgentCommand::SetName { reply, .. } => {
                        let _ = reply.send(false);
                    }
                    AgentCommand::Close { reply } => {
                        let _ = reply.send(());
                        break;
                    }
                    AgentCommand::Compact { reply, .. } => {
                        let _ = reply.send(Err("agent is not running".to_owned()));
                    }
                    AgentCommand::ReleaseCompaction { reply } => {
                        let _ = reply.send(());
                    }
                    AgentCommand::SendInput(_) => {}
                    AgentCommand::Interrupt { reply } => {
                        let _ = reply.send(InterruptOutcome::NotRunning);
                    }
                }
            }
        });

        AgentHandle {
            tx,
            accepting_input: accepting_input_task,
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
            turn_token_usage: None,
            context_breakdown: None,
            images: None,
        }
    }

    fn metadata_update(message_id: &str, total_tokens: u64) -> ChatEvent {
        ChatEvent::MessageMetadataUpdated(MessageMetadataUpdateData {
            message_id: ChatMessageId(message_id.to_owned()),
            model_info: Some(ModelInfo {
                model: "mock-model".to_owned(),
            }),
            token_usage: Some(TokenUsage {
                input_tokens: total_tokens / 2,
                output_tokens: total_tokens - (total_tokens / 2),
                total_tokens,
                cached_prompt_tokens: Some(0),
                cache_creation_input_tokens: Some(0),
                reasoning_tokens: Some(0),
            }),
            turn_token_usage: None,
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
        message.token_usage = Some(token_usage(total_tokens));
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
        message.token_usage = Some(token_usage(10));

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
    fn activity_stats_uses_event_seq_for_unidentified_token_usage() {
        let mut stats = AgentActivityStatsTracker::default();
        let mut first = assistant_message("first");
        first.token_usage = Some(token_usage(6));
        let mut second = assistant_message("second");
        second.token_usage = Some(token_usage(8));

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

    #[tokio::test]
    async fn relay_activity_stats_accumulate_subagent_turn_usage() {
        let dir = tempfile::tempdir().expect("tempdir");
        let session_store_path = dir.path().join("sessions.json");
        let session_store = Arc::new(Mutex::new(
            SessionStore::load(session_store_path).expect("load session store"),
        ));
        let start = test_agent_start("relay-stats-agent");
        let (status_handle, _status_rx) = AgentStatusHandle::new();
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let handle = spawn_relay_agent_actor(
            start.agent_id.clone(),
            start,
            event_rx,
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

        let stats = timeout(Duration::from_secs(1), async {
            loop {
                let event = output_rx
                    .recv()
                    .await
                    .expect("relay output stream should stay open");
                if event.kind == FrameKind::AgentActivityStats
                    && let Ok(payload) = event.parse_payload::<AgentActivityStatsPayload>()
                    && payload.stats.token_usage.total_tokens == 17
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
        assert_eq!(stats.last_output_line.as_deref(), Some("second"));
        drop(event_tx);
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

    #[test]
    fn name_generation_fallback_uses_prompt_when_generated_name_is_empty() {
        assert_eq!(
            name_generation_fallback("please fix login flow", "   "),
            "Fix Login Flow"
        );
    }

    #[test]
    fn name_generation_fallback_uses_prompt_when_generated_name_has_wrong_shape() {
        assert_eq!(
            name_generation_fallback("add project search filter", "project"),
            "Add Project Search Filter"
        );
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
                message: assistant_message("metadata arrives later"),
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
                message_id: Some("turn-1".to_owned()),
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
                message: assistant_message("hello world"),
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
                    message.token_usage.as_ref().map(|usage| usage.total_tokens),
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
                    message.token_usage.as_ref().map(|usage| usage.total_tokens),
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
                message_id: Some("turn-1".to_owned()),
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
                message: assistant_message("hello world"),
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
                    update.token_usage.as_ref().map(|usage| usage.total_tokens),
                    Some(42)
                );
            }
            other => panic!("expected MessageMetadataUpdated, got {other:?}"),
        }
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
                message_id: Some("turn-1".to_owned()),
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
    async fn replay_active_reasoning_uses_latest_message_id_before_live_end() {
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
                message_id: Some("turn-1".to_owned()),
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
                message_id: Some("turn-1".to_owned()),
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
                message: assistant_message("hello"),
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
            }),
        )
        .await;
        append_chat_event(
            canonical_stream,
            &mut event_log,
            &mut subscribers,
            &mut replay_state,
            &ChatEvent::StreamEnd(StreamEndData {
                message: assistant_message(""),
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
            Some(AgentBootstrapEvent::ChatEvent(ChatEvent::MessageAdded(_)))
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
        assert!(events.next().is_none());
        assert!(
            timeout(Duration::from_millis(50), rx.recv()).await.is_err(),
            "bootstrap tail should stay inside AgentBootstrap"
        );

        let window = session_history_window(&event_log, None, 10, None);
        assert!(matches!(
            window.events.as_slice(),
            [
                ChatEvent::ToolExecutionCompleted(_),
                ChatEvent::ToolRequest(_),
                ChatEvent::MessageAdded(_)
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

        assert!(
            !handle
                .send_input(AgentInput::SendMessage(protocol::SendMessagePayload {
                    message: "hello".to_string(),
                    images: None,
                    origin: None,
                    tool_response: None,
                }))
                .await
        );
        let snapshot = handle.snapshot();
        assert_eq!(snapshot.agent_id.0, "agent-failed");
        assert_eq!(snapshot.name, "Chat");

        let (tx, mut rx) = mpsc::unbounded_channel();
        let stream = Stream::new(StreamPath("/agent/agent-failed".to_string()), tx);
        assert!(handle.attach(stream).await);

        let mut events = recv_agent_bootstrap_events(&mut rx, "AgentBootstrap")
            .await
            .into_iter();
        let payload = match events.next() {
            Some(AgentBootstrapEvent::AgentError(payload)) => payload,
            other => panic!("expected AgentError in AgentBootstrap, got {other:?}"),
        };
        assert!(events.next().is_none());
        assert!(payload.fatal);
        assert_eq!(payload.message, "backend blew up");
    }
}
