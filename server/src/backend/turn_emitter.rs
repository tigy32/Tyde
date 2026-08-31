//! The single stateful boundary between backend events and Tyde chat events.
//!
//! Backends provide provider data; this emitter owns response presentation
//! identities, tool ownership, ordering, and terminal state. Illegal backend
//! transitions are contained without fabricating messages or tool calls, and
//! reported as an Error message when the turn ends.
//!
//! Containment used to be release-only; debug builds panicked instead. That
//! panic fired while the state mutex guard was held, which poisoned the mutex
//! and turned every later call into a panic, and it fired on whichever tokio
//! task the backend happened to be on, where tokio swallowed it. A debug build
//! therefore did not fail loudly on a violation — it silently stopped emitting.

use std::collections::HashMap;

use indexmap::IndexMap;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use uuid::Uuid;

use protocol::{
    ChatEvent, ChatMessageId, ContextBreakdown, ImageData, MessageMetadataUpdateData,
    MessageTokenUsage, ModelInfo, ModelRequestTokenUsage, ReasoningData, StreamEndData,
    StreamStartData, StreamTextDeltaData, TaskList, ToolExecutionCompletedData, ToolExecutionMode,
    ToolExecutionOutcome, ToolProgressData, ToolRequest, ToolRequestType, ToolUseData,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EmitterPolicy {
    /// Strict mode: protocol violations are recorded, invalid transitions are rejected,
    /// and any accumulated violations are emitted as an Error message to the user at the
    /// end of the turn. Used in pre-release/beta builds, tests, and conformance suites.
    Strict,
    /// Permissive mode: the emitter acts as an auto-healing normalizer. Violations are logged
    /// to tracing, but transitions are repaired on the fly (auto-opening responses on unannounced
    /// deltas, auto-closing responses on idle or overlapping starts, synthesizing missing tool
    /// declarations/requests) so downstream clients always receive a 100% valid event stream
    /// with zero dropped content. Used in stable release builds.
    Permissive,
}

pub struct TurnEmitter {
    inner: std::sync::Mutex<TurnEmitterState>,
}

struct TurnEmitterState {
    tx: mpsc::UnboundedSender<Value>,
    agent: String,
    policy: EmitterPolicy,
    typing_active: bool,
    current_response: Option<OpenResponse>,
    declared_tools: HashMap<String, DeclaredTool>,
    open_tool_requests: IndexMap<String, EmittedToolRequest>,
    completed_tool_requests: HashMap<String, CompletedToolRequest>,
    retired_tool_call_ids: IndexMap<String, CompletedToolRequest>,
    violations: Vec<String>,
}

const RETIRED_TOOL_CALL_LEDGER_CAP: usize = 1024;

struct EmittedToolRequest {
    tool_type: ToolRequestType,
    execution_mode: ToolExecutionMode,
}

struct CompletedToolRequest {
    tool_type: ToolRequestType,
    outcome: ToolExecutionOutcome,
}

#[derive(Clone)]
struct DeclaredTool {
    owner: ChatMessageId,
    declaration: ToolUseData,
}

#[derive(Clone, Copy)]
pub struct AgentName<'a>(pub &'a str);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResponseHandle {
    token: String,
    message_id: ChatMessageId,
}

impl ResponseHandle {
    pub(crate) fn message_id(&self) -> ChatMessageId {
        self.message_id.clone()
    }
}

struct OpenResponse {
    handle: ResponseHandle,
    message_id: ChatMessageId,
    model: Option<String>,
    content: String,
    reasoning: String,
    /// Where `stream_start` was called. A response left open at the end of a
    /// turn is reported to the user, and until this was recorded the report
    /// named no call site at all — `codex.rs` alone opens responses from eleven
    /// of them, so the report could not be acted on.
    opened_at: &'static std::panic::Location<'static>,
}

#[derive(Default)]
pub struct StreamEndPayload {
    pub content: String,
    pub model_info: Option<ModelInfo>,
    pub token_usage: Option<MessageTokenUsage>,
    pub reasoning: Option<ReasoningData>,
    pub tool_calls: Vec<ToolUseData>,
    pub context_breakdown: Option<ContextBreakdown>,
    pub images: Vec<ImageData>,
}

pub struct AssistantMessagePayload {
    pub message_id: Option<ChatMessageId>,
    pub content: String,
    pub reasoning: Option<ReasoningData>,
    pub tool_calls: Vec<ToolUseData>,
    pub model_info: Option<ModelInfo>,
    pub token_usage: Option<MessageTokenUsage>,
    pub context_breakdown: Option<ContextBreakdown>,
    pub images: Vec<ImageData>,
}

pub struct RetryAttemptPayload<'a> {
    pub attempt: u64,
    pub max_retries: u64,
    pub error: &'a str,
    pub backoff_ms: u64,
}

impl TurnEmitter {
    pub fn new(tx: mpsc::UnboundedSender<Value>) -> Self {
        Self::new_for_agent(tx, AgentName("assistant"))
    }

    pub fn new_for_agent(tx: mpsc::UnboundedSender<Value>, agent: AgentName<'_>) -> Self {
        let policy =
            if crate::host_release_version().is_some_and(|version| !version.is_prerelease()) {
                EmitterPolicy::Permissive
            } else {
                EmitterPolicy::Strict
            };
        Self::new_for_agent_with_policy(tx, agent, policy)
    }

    pub fn new_for_agent_with_policy(
        tx: mpsc::UnboundedSender<Value>,
        agent: AgentName<'_>,
        policy: EmitterPolicy,
    ) -> Self {
        Self {
            inner: std::sync::Mutex::new(TurnEmitterState {
                tx,
                agent: agent.0.to_owned(),
                policy,
                typing_active: false,
                current_response: None,
                declared_tools: HashMap::new(),
                open_tool_requests: IndexMap::new(),
                completed_tool_requests: HashMap::new(),
                retired_tool_call_ids: IndexMap::new(),
                violations: Vec::new(),
            }),
        }
    }

    #[track_caller]
    pub fn stream_start(&self, model: Option<&str>) -> ResponseHandle {
        self.lock()
            .stream_start(model, std::panic::Location::caller())
    }

    pub fn stream_delta(&self, response: &ResponseHandle, text: &str) {
        if text.is_empty() {
            return;
        }
        self.lock().stream_delta(response, text);
    }

    pub fn stream_reasoning_delta(&self, response: &ResponseHandle, text: &str) {
        if text.is_empty() {
            return;
        }
        self.lock().stream_reasoning_delta(response, text);
    }

    pub fn stream_end(&self, response: ResponseHandle, payload: StreamEndPayload) {
        self.lock().stream_end(&response, payload);
    }

    /// Declares tool calls on a response that is still streaming, so their cards
    /// appear while the tools run rather than when the response closes. A
    /// backend whose response boundary only arrives *after* its tools have
    /// finished — Codex, whose boundary is a `tokenUsage` change — would
    /// otherwise leave every card invisible for the duration of the command.
    ///
    /// The same declarations must still be passed to `stream_end`; that is what
    /// records them on the persisted message.
    pub fn declare_streaming_tools(
        &self,
        response: &ResponseHandle,
        declarations: Vec<ToolUseData>,
    ) {
        self.lock().declare_streaming_tools(response, declarations);
    }

    pub fn tool_request(&self, tool_call_id: &str, tool_type: ToolRequestType) -> bool {
        self.lock().tool_request(tool_call_id, tool_type)
    }

    pub fn tool_completed(&self, tool_call_id: &str, outcome: ToolExecutionOutcome) {
        self.lock().tool_completed(tool_call_id, outcome);
    }

    pub(crate) fn has_pending_tool_request(&self, tool_call_id: &str) -> bool {
        self.lock().is_tool_pending(tool_call_id)
    }

    pub(crate) fn has_known_tool_request(&self, tool_call_id: &str) -> bool {
        let state = self.lock();
        state.is_tool_pending(tool_call_id)
            || state.completed_tool_requests.contains_key(tool_call_id)
            || state.retired_tool_call_ids.contains_key(tool_call_id)
    }

    pub(crate) fn has_pending_background_tools(&self) -> bool {
        self.lock()
            .open_tool_requests
            .values()
            .any(|request| request.execution_mode == ToolExecutionMode::Background)
    }

    pub(crate) fn is_tool_background(&self, tool_call_id: &str) -> bool {
        self.lock()
            .open_tool_requests
            .get(tool_call_id)
            .is_some_and(|request| request.execution_mode == ToolExecutionMode::Background)
    }

    pub(crate) fn tool_request_name(&self, tool_call_id: &str) -> Option<String> {
        let state = self.lock();
        state
            .declared_tools
            .get(tool_call_id)
            .map(|declaration| declaration.declaration.name.clone())
    }

    pub(crate) fn tool_request_command(&self, tool_call_id: &str) -> Option<String> {
        let state = self.lock();
        let request = state.open_tool_requests.get(tool_call_id)?;
        let command = match &request.tool_type {
            ToolRequestType::RunCommand { command, .. } => Some(command.as_str()),
            _ => None,
        };
        command
            .map(str::trim)
            .filter(|command| !command.is_empty())
            .map(str::to_owned)
    }

    pub fn fail_pending_tool(&self, tool_call_id: &str, error: &str) -> bool {
        let mut state = self.lock();
        if !state.is_tool_pending(tool_call_id) {
            return false;
        }
        state.emit_tool_completion(
            tool_call_id,
            ToolExecutionOutcome::Failed {
                message: "Tool execution failed".to_owned(),
                details: Some(error.to_owned()),
                normalization_failure: None,
            },
        );
        true
    }

    pub fn cancel_pending_tool(&self, tool_call_id: &str, message: &str) -> bool {
        let mut state = self.lock();
        if !state.is_tool_pending(tool_call_id) {
            return false;
        }
        state.cancel_open_tool(tool_call_id, message);
        true
    }

    /// The foreground cards a cancel is about to report as `Cancelled`.
    ///
    /// A backend that can actually stop the work behind a card kills exactly
    /// this set, so the card and the process cannot disagree. Reporting a card
    /// cancelled while its process runs on is the bug this exists to prevent.
    pub fn open_foreground_tool_ids(&self) -> Vec<String> {
        self.lock()
            .open_tool_requests
            .iter()
            .filter(|(_, request)| request.execution_mode == ToolExecutionMode::Foreground)
            .map(|(tool_call_id, _)| tool_call_id.clone())
            .collect()
    }

    pub fn cancel_pending_foreground_tools(&self, message: &str) {
        let mut state = self.lock();
        let pending = state
            .open_tool_requests
            .iter()
            .filter(|(_, request)| request.execution_mode == ToolExecutionMode::Foreground)
            .map(|(tool_call_id, _)| tool_call_id.clone())
            .collect::<Vec<_>>();
        for tool_call_id in pending {
            state.cancel_open_tool(&tool_call_id, message);
        }
    }

    pub fn tool_progress(&self, data: &protocol::ToolProgressData) {
        if data.tool_call_id.trim().is_empty() {
            self.lock()
                .violation("empty_tool_call_id", "tool progress carried an empty id");
            return;
        }
        self.lock().send_tool_progress(data);
    }

    pub fn operation_cancelled(&self, message: &str) {
        self.lock().abort(message);
    }

    pub fn interrupt_acknowledged(&self, message: &str) {
        self.lock().abort(message);
    }

    pub(crate) fn close(&self, message: &str) {
        let mut state = self.lock();
        state.close(message);
        let (replacement, receiver) = mpsc::unbounded_channel();
        drop(receiver);
        state.tx = replacement;
    }

    pub fn user_message(&self, content: &str, images: Option<Vec<ImageData>>) {
        let mut state = self.lock();
        state.send_chat(ChatEvent::MessageAdded(protocol::ChatMessage {
            message_id: None,
            timestamp: now_ms(),
            sender: protocol::MessageSender::User,
            content: content.to_owned(),
            reasoning: None,
            tool_calls: Vec::new(),
            model_info: None,
            token_usage: None,
            context_breakdown: None,
            images,
        }));
    }

    pub fn system_message(&self, content: &str) {
        let mut state = self.lock();
        state.send_chat(ChatEvent::MessageAdded(simple_message(
            protocol::MessageSender::System,
            content,
        )));
    }

    pub fn warning_message(&self, content: &str) {
        let mut state = self.lock();
        state.send_chat(ChatEvent::MessageAdded(simple_message(
            protocol::MessageSender::Warning,
            content,
        )));
    }

    pub fn error_message(&self, content: &str) {
        let mut state = self.lock();
        state.send_chat(ChatEvent::MessageAdded(simple_message(
            protocol::MessageSender::Error,
            content,
        )));
    }

    pub fn replay_assistant_message(&self, payload: AssistantMessagePayload) {
        self.lock().assistant_message(payload);
    }

    pub fn message_metadata_updated(&self, update: MessageMetadataUpdateData) {
        self.lock().message_metadata_updated(update);
    }

    pub fn model_request_token_usage(&self, usage: &ModelRequestTokenUsage) {
        let data = serde_json::to_value(usage).expect("model request token usage must serialize");
        self.lock().send(json!({
            "kind": "ModelRequestTokenUsage",
            "data": data,
        }));
    }

    pub fn total_only_token_usage(&self, total_tokens: u64) {
        self.lock().send(json!({
            "kind": "TotalOnlyTokenUsage",
            "data": { "total_tokens": total_tokens },
        }));
    }

    pub(crate) fn compaction_event(&self, event: &super::compaction::BackendCompactionEvent) {
        let data = serde_json::to_value(event).expect("backend compaction event must serialize");
        self.lock().send(json!({
            "kind": "BackendCompaction",
            "data": data,
        }));
    }

    #[track_caller]
    pub fn typing_status_changed(&self, typing: bool) {
        let idle_caller = std::panic::Location::caller();
        let mut state = self.lock();
        if typing == state.typing_active {
            return;
        }
        if !typing {
            // Both ends of the report matter: which terminal path declared the
            // turn over, and which `stream_start` opened the response it walked
            // away from. The volume says whether the user lost real output or
            // only an empty shell.
            let abandoned = state.current_response.as_ref().map(|open| {
                (
                    open.handle.clone(),
                    format!(
                        "typing ended at {idle_caller} before the response opened at {} reached \
                         StreamEnd, discarding {} characters of content and {} of reasoning",
                        open.opened_at,
                        open.content.chars().count(),
                        open.reasoning.chars().count(),
                    ),
                )
            });
            if let Some((response, detail)) = abandoned {
                state.violation("idle_with_open_response", detail);
                if state.policy == EmitterPolicy::Permissive {
                    state.auto_end_current_response();
                } else {
                    state.discard_open_response(&response);
                }
            }
            let foreground_tools = state
                .open_tool_requests
                .iter()
                .filter(|(_, request)| {
                    request.execution_mode == ToolExecutionMode::Foreground
                        && !awaits_user_response(&request.tool_type)
                })
                .map(|(tool_call_id, _)| tool_call_id.clone())
                .collect::<Vec<_>>();
            if let Some(tool_call_id) = foreground_tools.first() {
                state.violation(
                    "idle_with_foreground_tool",
                    format!("typing ended while foreground tool '{tool_call_id}' was still open"),
                );
            }
            for tool_call_id in foreground_tools {
                state.cancel_open_tool(
                    &tool_call_id,
                    "Backend became idle before tool execution completed",
                );
            }
            state.retire_completed_tools();
            state.report_violations();
        }
        state.send_chat(ChatEvent::TypingStatusChanged(typing));
        state.typing_active = typing;
    }

    pub fn task_update(&self, tasks: &TaskList) {
        self.lock().send_chat(ChatEvent::TaskUpdate(tasks.clone()));
    }

    pub fn retry_attempt(&self, payload: RetryAttemptPayload<'_>) {
        self.lock()
            .send_chat(ChatEvent::RetryAttempt(protocol::RetryAttemptData {
                attempt: payload.attempt,
                max_retries: payload.max_retries,
                error: payload.error.to_owned(),
                backoff_ms: payload.backoff_ms,
            }));
    }

    pub fn session_started(&self, session_id: &str) {
        self.lock().send(json!({
            "kind": "SessionStarted",
            "data": { "session_id": session_id },
        }));
    }

    pub fn backend_error(&self, message: &str) {
        self.lock().send(json!({
            "kind": "Error",
            "data": message,
        }));
    }

    pub fn conversation_cleared(&self) {
        let mut state = self.lock();
        state.reset_turn_state();
        state.typing_active = false;
        state.open_tool_requests.clear();
        state.declared_tools.clear();
        state.completed_tool_requests.clear();
        state.retired_tool_call_ids.clear();
        state.send(json!({ "kind": "ConversationCleared" }));
    }

    pub fn settings(&self, data: Value) {
        self.lock().send(json!({
            "kind": "Settings",
            "data": data,
        }));
    }

    pub fn sessions_list(&self, sessions: Vec<Value>) {
        self.lock().send(json!({
            "kind": "SessionsList",
            "data": { "sessions": sessions },
        }));
    }

    pub fn profiles_list(&self, profiles: Vec<Value>) {
        self.lock().send(json!({
            "kind": "ProfilesList",
            "data": { "profiles": profiles },
        }));
    }

    pub fn module_schemas(&self, schemas: Vec<Value>) {
        self.lock().send(json!({
            "kind": "ModuleSchemas",
            "data": { "schemas": schemas },
        }));
    }

    pub fn models_list(&self, models: Vec<Value>) {
        self.lock().send(json!({
            "kind": "ModelsList",
            "data": { "models": models },
        }));
    }

    pub fn subprocess_stderr(&self, line: &str) {
        self.lock().send(json!({
            "kind": "SubprocessStderr",
            "data": line,
        }));
    }

    pub fn subprocess_exit(&self, exit_code: Option<i32>) {
        self.lock().send(json!({
            "kind": "SubprocessExit",
            "data": { "exit_code": exit_code },
        }));
    }

    pub fn is_stream_open(&self) -> bool {
        self.lock().current_response.is_some()
    }

    /// The response this turn is streaming into, opening one if none is.
    ///
    /// Response lifetime belongs to the emitter: it retires the open response at
    /// `stream_end`, and drops it if the turn goes idle while it is still open.
    /// A backend that keeps its own `Option<ResponseHandle>` is caching state it
    /// does not own, and every write through a copy the emitter has since
    /// retired is rejected without the backend being told. Measured on Codex: a
    /// turn went idle with a response still open, and the next turn's entire
    /// answer arrived as 71 rejected deltas and a rejected `stream_end` — the
    /// user got an error banner where their reply should have been. Ask the
    /// owner rather than keeping a copy of what it owns.
    #[track_caller]
    pub fn ensure_open_response(&self, model: Option<&str>) -> ResponseHandle {
        let caller = std::panic::Location::caller();
        let mut state = self.lock();
        if let Some(open) = state.current_response.as_ref() {
            return open.handle.clone();
        }
        state.stream_start(model, caller)
    }

    /// The response this turn is streaming into, or `None` if none is open.
    ///
    /// The read for a caller that is about to *close* a response. Unlike
    /// [`Self::ensure_open_response`] it never mints one, because opening a
    /// response only to end it publishes an empty message to the user.
    pub fn open_response(&self) -> Option<ResponseHandle> {
        self.lock()
            .current_response
            .as_ref()
            .map(|open| open.handle.clone())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, TurnEmitterState> {
        self.inner.lock().expect("TurnEmitter mutex poisoned")
    }
}

impl TurnEmitterState {
    fn send(&self, event: Value) {
        let _ = self.tx.send(event);
    }

    fn send_chat(&mut self, event: ChatEvent) {
        match serde_json::to_value(event) {
            Ok(event) => self.send(event),
            Err(error) => self.violation(
                "chat_event_serialization",
                format!("failed to serialize a typed chat event: {error}"),
            ),
        }
    }

    fn violation(&mut self, code: &'static str, detail: impl std::fmt::Display) {
        let detail = detail.to_string();
        tracing::error!(
            violation = code,
            detail,
            "Backend event transition violated the chat protocol"
        );
        self.violations.push(format!("[{code}] {detail}"));
    }

    /// Emitted before the turn's `TypingStatusChanged(false)` so it lands inside
    /// the turn that produced it.
    fn report_violations(&mut self) {
        if self.violations.is_empty() {
            return;
        }
        let violations = std::mem::take(&mut self.violations);
        if self.policy != EmitterPolicy::Strict {
            tracing::debug!(
                count = violations.len(),
                "Suppressing user-facing protocol violation report for a permissive emitter"
            );
            return;
        }
        let total = violations.len();
        // A wedged response repeats one violation per delta — 71 identical lines
        // in a real report. The count is the information; the repetition only
        // buries the other entries, which are the ones that say what happened.
        let mut runs: Vec<(String, usize)> = Vec::new();
        for violation in violations {
            match runs.last_mut() {
                Some((seen, count)) if *seen == violation => *count += 1,
                _ => runs.push((violation, 1)),
            }
        }
        let reported = runs
            .into_iter()
            .map(|(violation, count)| match count {
                1 => violation,
                count => format!("{violation} (x{count})"),
            })
            .collect::<Vec<_>>()
            .join("\n");
        let content = format!(
            "The backend sent {total} malformed event(s) during this turn, so the conversation \
             above may be missing tool cards or responses:\n{reported}"
        );
        self.send_chat(ChatEvent::MessageAdded(simple_message(
            protocol::MessageSender::Error,
            &content,
        )));
    }

    fn auto_end_current_response(&mut self) {
        if let Some(open) = self.current_response.take() {
            let mut message = self.build_stream_end_message(StreamEndPayload::default(), open);
            let owner = message
                .message_id
                .clone()
                .expect("stream response has a presentation id");
            message.tool_calls = self.sanitize_tool_declarations(
                owner,
                message.content.chars().count() as u64,
                std::mem::take(&mut message.tool_calls),
            );
            self.send_chat(ChatEvent::StreamEnd(StreamEndData { message }));
        }
    }

    fn stream_start(
        &mut self,
        model: Option<&str>,
        caller: &'static std::panic::Location<'static>,
    ) -> ResponseHandle {
        tracing::debug!(
            agent = self.agent.as_str(),
            caller = %caller,
            "Opening a backend response"
        );
        if let Some((response, previous_caller)) = self
            .current_response
            .as_ref()
            .map(|response| (response.handle.clone(), response.opened_at))
        {
            self.violation(
                "overlapping_response",
                format!(
                    "a response started at {caller} while the response opened at \
                     {previous_caller} was still open"
                ),
            );
            if self.policy == EmitterPolicy::Permissive {
                self.auto_end_current_response();
            } else {
                self.discard_open_response(&response);
            }
        }

        let message_id = ChatMessageId(Uuid::new_v4().to_string());
        let response = ResponseHandle {
            token: Uuid::new_v4().to_string(),
            message_id: message_id.clone(),
        };
        self.current_response = Some(OpenResponse {
            handle: response.clone(),
            message_id,
            model: model.map(str::to_owned),
            content: String::new(),
            reasoning: String::new(),
            opened_at: caller,
        });
        self.send_chat(ChatEvent::StreamStart(StreamStartData {
            agent: self.agent.clone(),
            model: model.map(str::to_owned),
        }));
        response
    }

    fn stream_delta(&mut self, response: &ResponseHandle, text: &str) {
        if !self.ensure_accepted_response(response, "response_delta") {
            return;
        }
        self.current_response
            .as_mut()
            .expect("validated open response")
            .content
            .push_str(text);
        self.send_chat(ChatEvent::StreamDelta(StreamTextDeltaData {
            text: text.to_owned(),
        }));
    }

    fn stream_reasoning_delta(&mut self, response: &ResponseHandle, text: &str) {
        if !self.ensure_accepted_response(response, "response_reasoning_delta") {
            return;
        }
        self.current_response
            .as_mut()
            .expect("validated open response")
            .reasoning
            .push_str(text);
        self.send_chat(ChatEvent::StreamReasoningDelta(StreamTextDeltaData {
            text: text.to_owned(),
        }));
    }

    fn stream_end(&mut self, response: &ResponseHandle, payload: StreamEndPayload) {
        if let Some(open) = self.current_response.take() {
            if &open.handle != response {
                self.violation(
                    "response_end",
                    "event used a stale or foreign response handle",
                );
                if self.policy == EmitterPolicy::Strict {
                    self.current_response = Some(open);
                    return;
                }
            }
            let mut message = self.build_stream_end_message(payload, open);
            let owner = message
                .message_id
                .clone()
                .expect("stream response has a presentation id");
            message.tool_calls = self.sanitize_tool_declarations(
                owner,
                message.content.chars().count() as u64,
                std::mem::take(&mut message.tool_calls),
            );
            self.send_chat(ChatEvent::StreamEnd(StreamEndData { message }));
            return;
        }

        self.violation("response_end", "event arrived while no response was open");
        if self.policy == EmitterPolicy::Permissive {
            let message_id = if response.message_id.0.trim().is_empty() {
                ChatMessageId(Uuid::new_v4().to_string())
            } else {
                response.message_id.clone()
            };
            let mut message = protocol::ChatMessage {
                message_id: Some(message_id.clone()),
                timestamp: now_ms(),
                sender: protocol::MessageSender::Assistant {
                    agent: self.agent.clone(),
                },
                content: payload.content,
                reasoning: payload.reasoning,
                tool_calls: payload.tool_calls,
                model_info: payload.model_info,
                token_usage: payload.token_usage,
                context_breakdown: payload.context_breakdown,
                images: (!payload.images.is_empty()).then_some(payload.images),
            };
            let content_len = message.content.chars().count() as u64;
            message.tool_calls = self.sanitize_tool_declarations(
                message_id,
                content_len,
                std::mem::take(&mut message.tool_calls),
            );
            self.send_chat(ChatEvent::StreamEnd(StreamEndData { message }));
        }
    }

    fn declare_streaming_tools(
        &mut self,
        response: &ResponseHandle,
        declarations: Vec<ToolUseData>,
    ) {
        if let Some((owner, content_len, is_same_handle)) =
            self.current_response.as_ref().map(|open| {
                (
                    open.message_id.clone(),
                    open.content.chars().count() as u64,
                    &open.handle == response,
                )
            })
        {
            if !is_same_handle {
                self.violation(
                    "streaming_tool_declaration",
                    "event used a stale or foreign response handle",
                );
                if self.policy == EmitterPolicy::Strict {
                    return;
                }
            }
            self.sanitize_tool_declarations(owner, content_len, declarations);
            return;
        }

        self.violation(
            "streaming_tool_declaration",
            "event arrived while no response was open",
        );
        if self.policy == EmitterPolicy::Permissive {
            let owner = if response.message_id.0.trim().is_empty() {
                ChatMessageId(Uuid::new_v4().to_string())
            } else {
                response.message_id.clone()
            };
            self.sanitize_tool_declarations(owner, 0, declarations);
        }
    }

    fn discard_open_response(&mut self, response: &ResponseHandle) {
        if self
            .current_response
            .as_ref()
            .is_some_and(|open| &open.handle == response)
        {
            self.current_response = None;
        }
    }

    fn ensure_accepted_response(&mut self, response: &ResponseHandle, event: &'static str) -> bool {
        if let Some(open) = &self.current_response {
            if &open.handle == response {
                return true;
            }
            self.violation(event, "event used a stale or foreign response handle");
            return self.policy == EmitterPolicy::Permissive;
        }

        self.violation(event, "event arrived while no response was open");
        if self.policy == EmitterPolicy::Permissive {
            let message_id = if response.message_id.0.trim().is_empty() {
                ChatMessageId(Uuid::new_v4().to_string())
            } else {
                response.message_id.clone()
            };
            self.current_response = Some(OpenResponse {
                handle: response.clone(),
                message_id,
                model: None,
                content: String::new(),
                reasoning: String::new(),
                opened_at: std::panic::Location::caller(),
            });
            self.send_chat(ChatEvent::StreamStart(StreamStartData {
                agent: self.agent.clone(),
                model: None,
            }));
            return true;
        }
        false
    }

    fn build_stream_end_message(
        &self,
        mut payload: StreamEndPayload,
        response: OpenResponse,
    ) -> protocol::ChatMessage {
        if payload.content.is_empty() {
            payload.content = response.content;
        }
        let reasoning = payload.reasoning.or_else(|| {
            (!response.reasoning.is_empty()).then_some(ReasoningData {
                text: response.reasoning,
                tokens: None,
                signature: None,
                blob: None,
            })
        });
        let model_info = payload
            .model_info
            .or_else(|| response.model.map(|model| ModelInfo { model }));

        protocol::ChatMessage {
            message_id: Some(response.message_id),
            timestamp: now_ms(),
            sender: protocol::MessageSender::Assistant {
                agent: self.agent.clone(),
            },
            content: payload.content,
            reasoning,
            tool_calls: payload.tool_calls,
            model_info,
            token_usage: payload.token_usage,
            context_breakdown: payload.context_breakdown,
            images: (!payload.images.is_empty()).then_some(payload.images),
        }
    }

    fn assistant_message(&mut self, mut payload: AssistantMessagePayload) {
        let message_id = match payload.message_id.take() {
            Some(message_id) if !message_id.0.trim().is_empty() => message_id,
            Some(_) => {
                self.violation(
                    "empty_assistant_message_id",
                    "assistant replay carried an empty presentation id",
                );
                ChatMessageId(Uuid::new_v4().to_string())
            }
            None => ChatMessageId(Uuid::new_v4().to_string()),
        };
        let mut message = self.build_assistant_message(payload, message_id.clone());
        message.tool_calls = self.sanitize_tool_declarations(
            message_id,
            message.content.chars().count() as u64,
            std::mem::take(&mut message.tool_calls),
        );
        self.send_chat(ChatEvent::MessageAdded(message));
    }

    fn build_assistant_message(
        &self,
        payload: AssistantMessagePayload,
        message_id: ChatMessageId,
    ) -> protocol::ChatMessage {
        protocol::ChatMessage {
            message_id: Some(message_id),
            timestamp: now_ms(),
            sender: protocol::MessageSender::Assistant {
                agent: self.agent.clone(),
            },
            content: payload.content,
            reasoning: payload.reasoning,
            tool_calls: payload.tool_calls,
            model_info: payload.model_info,
            token_usage: payload.token_usage,
            context_breakdown: payload.context_breakdown,
            images: (!payload.images.is_empty()).then_some(payload.images),
        }
    }

    fn sanitize_tool_declarations(
        &mut self,
        owner: ChatMessageId,
        content_len: u64,
        declarations: Vec<ToolUseData>,
    ) -> Vec<ToolUseData> {
        let mut accepted = Vec::with_capacity(declarations.len());
        for mut declaration in declarations {
            if declaration.tool_call_id.trim().is_empty() {
                self.violation(
                    "empty_tool_call_id",
                    "response declared a tool call with an empty id",
                );
                if self.policy == EmitterPolicy::Permissive {
                    declaration.tool_call_id = format!("synth_tool_{}", Uuid::new_v4());
                } else {
                    continue;
                }
            }
            if declaration
                .content_offset
                .is_some_and(|offset| u64::from(offset) > content_len)
            {
                self.violation(
                    "invalid_tool_content_offset",
                    format!(
                        "tool '{}' declared an offset beyond its response content",
                        declaration.tool_call_id
                    ),
                );
                declaration.content_offset = None;
            }
            let tool_call_id = declaration.tool_call_id.clone();
            if let Some(existing) = self.declared_tools.get(&tool_call_id) {
                if existing.owner == owner
                    && existing.declaration.name == declaration.name
                    && existing.declaration.arguments == declaration.arguments
                    && existing.declaration.content_offset == declaration.content_offset
                {
                    // Already registered by `declare_streaming_tools`, which is how
                    // the card appeared while the tool ran. It still has to reach
                    // the persisted message, or the call survives only in the live
                    // view and vanishes when history is replayed.
                    accepted.push(declaration);
                    continue;
                }
                self.violation(
                    "conflicting_tool_declaration",
                    format!(
                        "tool call '{tool_call_id}' was declared more than once with different data"
                    ),
                );
                if self.policy == EmitterPolicy::Permissive {
                    accepted.push(declaration);
                }
                continue;
            }
            if self.completed_tool_requests.contains_key(&tool_call_id)
                || self.retired_tool_call_ids.contains_key(&tool_call_id)
            {
                self.violation(
                    "reused_tool_call_id",
                    format!("completed tool call id '{tool_call_id}' was declared again"),
                );
                if self.policy == EmitterPolicy::Permissive {
                    accepted.push(declaration);
                }
                continue;
            }
            self.declared_tools.insert(
                tool_call_id,
                DeclaredTool {
                    owner: owner.clone(),
                    declaration: declaration.clone(),
                },
            );
            accepted.push(declaration);
        }
        accepted
    }

    fn message_metadata_updated(&mut self, update: MessageMetadataUpdateData) {
        if update.message_id.0.trim().is_empty() {
            self.violation(
                "empty_message_metadata_id",
                "message metadata carried an empty presentation id",
            );
            return;
        }
        if update.model_info.is_none()
            && update.token_usage.is_none()
            && update.context_breakdown.is_none()
        {
            return;
        }
        self.send_chat(ChatEvent::MessageMetadataUpdated(update));
    }

    fn tool_request(&mut self, tool_call_id: &str, tool_type: ToolRequestType) -> bool {
        if tool_call_id.trim().is_empty() {
            self.violation("empty_tool_call_id", "tool request carried an empty id");
            return false;
        }
        if let Some(existing) = self
            .completed_tool_requests
            .get(tool_call_id)
            .or_else(|| self.retired_tool_call_ids.get(tool_call_id))
        {
            if existing.tool_type != tool_type {
                self.violation(
                    "conflicting_duplicate_request",
                    format!("completed tool request '{tool_call_id}' was repeated with different executable data"),
                );
            }
            return false;
        }
        if !self.declared_tools.contains_key(tool_call_id) {
            self.violation(
                "undeclared_tool_request",
                format!("tool request '{tool_call_id}' was not declared by an assistant response"),
            );
            if self.policy == EmitterPolicy::Permissive {
                let owner = self
                    .current_response
                    .as_ref()
                    .map(|open| open.message_id.clone())
                    .unwrap_or_else(|| ChatMessageId(Uuid::new_v4().to_string()));
                let tool_name = tool_type_default_name(&tool_type);
                self.declared_tools.insert(
                    tool_call_id.to_owned(),
                    DeclaredTool {
                        owner,
                        declaration: ToolUseData {
                            tool_call_id: tool_call_id.to_owned(),
                            name: tool_name.to_owned(),
                            arguments: serde_json::Value::Null,
                            content_offset: None,
                        },
                    },
                );
            } else {
                return false;
            }
        }
        if let Some(existing) = self.open_tool_requests.get(tool_call_id) {
            if existing.tool_type == tool_type {
                return true;
            }
            self.violation(
                "conflicting_tool_request",
                format!(
                    "tool request '{tool_call_id}' was repeated with different executable data"
                ),
            );
            return self.policy == EmitterPolicy::Permissive;
        }
        self.open_tool_requests.insert(
            tool_call_id.to_owned(),
            EmittedToolRequest {
                tool_type: tool_type.clone(),
                execution_mode: ToolExecutionMode::Foreground,
            },
        );
        let tool_name = self
            .declared_tools
            .get(tool_call_id)
            .expect("validated declared tool")
            .declaration
            .name
            .clone();
        self.send_chat(ChatEvent::ToolRequest(ToolRequest {
            tool_call_id: tool_call_id.to_owned(),
            tool_name,
            tool_type,
        }));
        true
    }

    fn tool_completed(&mut self, tool_call_id: &str, outcome: ToolExecutionOutcome) {
        if let Some(existing) = self.completed_tool_requests.get(tool_call_id) {
            if existing.outcome != outcome {
                self.violation(
                    "conflicting_duplicate_completion",
                    format!(
                        "tool '{tool_call_id}' completed twice with different outcomes: first={:?}, second={outcome:?}",
                        existing.outcome
                    ),
                );
            }
            return;
        }
        if let Some(existing) = self.retired_tool_call_ids.get(tool_call_id) {
            if existing.outcome != outcome {
                self.violation(
                    "conflicting_late_completion",
                    format!(
                        "retired tool '{tool_call_id}' changed outcome from {:?} to {outcome:?}",
                        existing.outcome
                    ),
                );
            }
            return;
        }
        if !self.open_tool_requests.contains_key(tool_call_id) {
            self.violation(
                "completion_without_request",
                format!("tool completion '{tool_call_id}' had no open request"),
            );
            if self.policy == EmitterPolicy::Permissive {
                let synthetic_type = ToolRequestType::Other {
                    args: serde_json::Value::Null,
                };
                let _ = self.tool_request(tool_call_id, synthetic_type);
            } else {
                return;
            }
        }
        self.emit_tool_completion(tool_call_id, outcome);
    }

    fn send_tool_progress(&mut self, data: &ToolProgressData) {
        let is_background = self
            .open_tool_requests
            .get(&data.tool_call_id)
            .map(|r| r.execution_mode == ToolExecutionMode::Background);

        let Some(is_background) = is_background else {
            let finished = self
                .completed_tool_requests
                .contains_key(&data.tool_call_id)
                || self.retired_tool_call_ids.contains_key(&data.tool_call_id);
            if !finished {
                self.violation(
                    "progress_without_request",
                    format!("tool progress '{}' had no open request", data.tool_call_id),
                );
                if self.policy == EmitterPolicy::Permissive {
                    let synthetic_type = ToolRequestType::Other {
                        args: serde_json::Value::Null,
                    };
                    let _ = self.tool_request(&data.tool_call_id, synthetic_type);
                    if let Some(request) = self.open_tool_requests.get_mut(&data.tool_call_id) {
                        request.execution_mode = data.execution_mode;
                    }
                    self.send_chat(ChatEvent::ToolProgress(data.clone()));
                }
            } else if data.execution_mode == ToolExecutionMode::Background {
                // Backgrounded work outlives the call that launched it: the tool
                // returns a handle as soon as the work is handed off, so every
                // later report — including the one saying it finished — arrives
                // after the card closed. Dropping those freezes the card on its
                // first snapshot for good.
                self.send_chat(ChatEvent::ToolProgress(data.clone()));
            } else {
                self.violation(
                    "progress_after_completion",
                    format!(
                        "tool progress followed completion for '{}'",
                        data.tool_call_id
                    ),
                );
            }
            return;
        };
        if is_background && data.execution_mode == ToolExecutionMode::Foreground {
            self.violation(
                "background_tool_returned_to_foreground",
                format!(
                    "tool '{}' moved from background back to foreground",
                    data.tool_call_id
                ),
            );
            if self.policy == EmitterPolicy::Strict {
                return;
            }
        }
        if data.execution_mode == ToolExecutionMode::Background
            && let Some(request) = self.open_tool_requests.get_mut(&data.tool_call_id)
        {
            request.execution_mode = ToolExecutionMode::Background;
        }
        self.send_chat(ChatEvent::ToolProgress(data.clone()));
    }

    fn emit_tool_completion(&mut self, tool_call_id: &str, outcome: ToolExecutionOutcome) {
        let request = self
            .open_tool_requests
            .shift_remove(tool_call_id)
            .expect("validated open tool request");
        self.completed_tool_requests.insert(
            tool_call_id.to_owned(),
            CompletedToolRequest {
                tool_type: request.tool_type,
                outcome: outcome.clone(),
            },
        );
        // The declaration outlives the completion on purpose. A tool declared on
        // a still-open response can finish before that response closes, and
        // `stream_end` has to re-declare it to record the call on the persisted
        // message. Dropping it here made that re-declaration look like a reused
        // id, so the message ended up declaring nothing.
        self.send_chat(ChatEvent::ToolExecutionCompleted(
            ToolExecutionCompletedData {
                tool_call_id: tool_call_id.to_owned(),
                outcome,
            },
        ));
    }

    fn abort(&mut self, cancellation_message: &str) {
        if let Some(response) = self
            .current_response
            .as_ref()
            .map(|response| response.handle.clone())
        {
            self.discard_open_response(&response);
        }
        self.complete_pending_tools_as_cancelled("Tool execution was cancelled by user");
        self.send_chat(ChatEvent::OperationCancelled(
            protocol::OperationCancelledData {
                message: cancellation_message.to_owned(),
            },
        ));
        self.report_violations();
        self.send_chat(ChatEvent::TypingStatusChanged(false));
        self.typing_active = false;
        self.reset_turn_state();
    }

    fn close(&mut self, message: &str) {
        let foreground_active = self.current_response.is_some()
            || self.typing_active
            || self
                .open_tool_requests
                .values()
                .any(|request| request.execution_mode == ToolExecutionMode::Foreground);
        if let Some(response) = self
            .current_response
            .as_ref()
            .map(|response| response.handle.clone())
        {
            self.discard_open_response(&response);
        }
        self.complete_pending_tools_as_cancelled(message);
        let background = self
            .open_tool_requests
            .iter()
            .filter(|(_, request)| request.execution_mode == ToolExecutionMode::Background)
            .map(|(tool_call_id, _)| tool_call_id.clone())
            .collect::<Vec<_>>();
        for tool_call_id in background {
            self.cancel_open_tool(&tool_call_id, message);
        }
        // Outside the `foreground_active` branch: closing an idle agent is the
        // one path where violations recorded after the last turn ended would
        // otherwise never be reported at all.
        self.report_violations();
        if foreground_active {
            self.send_chat(ChatEvent::OperationCancelled(
                protocol::OperationCancelledData {
                    message: message.to_owned(),
                },
            ));
            self.send_chat(ChatEvent::TypingStatusChanged(false));
            self.typing_active = false;
        }
        self.reset_turn_state();
    }

    fn complete_pending_tools_as_cancelled(&mut self, detailed_message: &str) {
        let pending = self
            .open_tool_requests
            .iter()
            .filter(|(_, request)| request.execution_mode == ToolExecutionMode::Foreground)
            .map(|(tool_call_id, _)| tool_call_id.clone())
            .collect::<Vec<_>>();
        for tool_call_id in pending {
            self.cancel_open_tool(&tool_call_id, detailed_message);
        }
    }

    fn cancel_open_tool(&mut self, tool_call_id: &str, message: &str) {
        self.emit_tool_completion(
            tool_call_id,
            ToolExecutionOutcome::Cancelled {
                message: message.to_owned(),
            },
        );
    }

    fn is_tool_pending(&self, tool_call_id: &str) -> bool {
        self.open_tool_requests.contains_key(tool_call_id)
    }

    fn reset_turn_state(&mut self) {
        self.current_response = None;
        self.retire_completed_tools();
        self.declared_tools
            .retain(|tool_call_id, _| self.open_tool_requests.contains_key(tool_call_id));
    }

    fn retire_completed_tools(&mut self) {
        let completed = std::mem::take(&mut self.completed_tool_requests);
        for (tool_call_id, outcome) in completed {
            self.retired_tool_call_ids.insert(tool_call_id, outcome);
        }
        while self.retired_tool_call_ids.len() > RETIRED_TOOL_CALL_LEDGER_CAP {
            self.retired_tool_call_ids.shift_remove_index(0);
        }
    }
}

/// Is this tool waiting on a human rather than on the machine?
///
/// A question or a plan approval is the one kind of foreground tool that is
/// *supposed* to still be open when the turn goes idle: the turn ends precisely
/// so the user can act on the card. Treating that as a stuck tool cancels the
/// card the user was asked to answer, and the answer that arrives afterwards is
/// then rejected as a completion for an id this emitter has already retired —
/// leaving the provider blocked on a response that can no longer be sent.
fn awaits_user_response(tool_type: &ToolRequestType) -> bool {
    matches!(
        tool_type,
        ToolRequestType::AskUserQuestion { .. } | ToolRequestType::ExitPlanMode { .. }
    )
}

fn tool_type_default_name(tool_type: &ToolRequestType) -> &'static str {
    match tool_type {
        ToolRequestType::ModifyFile { .. } => "modify_file",
        ToolRequestType::RunCommand { .. } => "run_command",
        ToolRequestType::ReadFiles { .. } => "read_files",
        ToolRequestType::SearchTypes { .. } => "search_types",
        ToolRequestType::GetTypeDocs { .. } => "get_type_docs",
        ToolRequestType::AskUserQuestion { .. } => "ask_question",
        ToolRequestType::ExitPlanMode { .. } => "exit_plan_mode",
        ToolRequestType::AgentSpawn { .. } => "agent_spawn",
        ToolRequestType::GenerateImage { .. } => "generate_image",
        ToolRequestType::WebSearch { .. } => "web_search",
        ToolRequestType::ViewImage { .. } => "view_image",
        ToolRequestType::Sleep { .. } => "sleep",
        ToolRequestType::TydeSendAgentMessage { .. } => "send_agent_message",
        ToolRequestType::TydeAwaitAgents { .. } => "await_agents",
        ToolRequestType::Other { .. } => "tool",
    }
}

fn simple_message(sender: protocol::MessageSender, content: &str) -> protocol::ChatMessage {
    protocol::ChatMessage {
        message_id: None,
        timestamp: now_ms(),
        sender,
        content: content.to_owned(),
        reasoning: None,
        tool_calls: Vec::new(),
        model_info: None,
        token_usage: None,
        context_breakdown: None,
        images: None,
    }
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn emitter() -> (TurnEmitter, mpsc::UnboundedReceiver<Value>) {
        emitter_with_policy(EmitterPolicy::Strict)
    }

    fn emitter_with_policy(policy: EmitterPolicy) -> (TurnEmitter, mpsc::UnboundedReceiver<Value>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            TurnEmitter::new_for_agent_with_policy(tx, AgentName("assistant"), policy),
            rx,
        )
    }

    fn drain_events(rx: &mut mpsc::UnboundedReceiver<Value>) -> Vec<Value> {
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        events
    }

    fn violation_reports(events: &[Value]) -> Vec<String> {
        let mut reports = Vec::new();
        for event in events {
            if event.get("kind").and_then(Value::as_str) == Some("MessageAdded")
                && let Some(content) = event.pointer("/data/content").and_then(Value::as_str)
                && content.starts_with("The backend sent")
            {
                reports.push(content.to_owned());
            }
        }
        reports
    }

    /// The violation report the user actually sees, drained from the wire.
    fn reports(rx: &mut mpsc::UnboundedReceiver<Value>) -> Vec<String> {
        violation_reports(&drain_events(rx))
    }

    #[test]
    fn an_abandoned_response_names_where_it_opened_where_it_ended_and_what_was_lost() {
        let (emitter, mut rx) = emitter();
        emitter.typing_status_changed(true);
        let opened_at = line!() + 1;
        let response = emitter.stream_start(Some("test-model"));
        emitter.stream_delta(&response, "twelve chars");
        emitter.stream_reasoning_delta(&response, "abc");
        let ended_at = line!() + 1;
        emitter.typing_status_changed(false);

        let reports = reports(&mut rx);
        assert_eq!(reports.len(), 1, "expected one report, got {reports:?}");
        let report = &reports[0];
        assert!(
            report.contains("[idle_with_open_response]"),
            "report lost its violation code: {report}"
        );
        assert!(
            report.contains(&format!("turn_emitter.rs:{opened_at}:")),
            "report does not name the stream_start call site: {report}"
        );
        assert!(
            report.contains(&format!("turn_emitter.rs:{ended_at}:")),
            "report does not name the terminal call site: {report}"
        );
        assert!(
            report.contains("discarding 12 characters of content and 3 of reasoning"),
            "report does not say how much was thrown away: {report}"
        );
    }

    /// The wedge, stated as the property that makes it impossible: after a turn
    /// abandons a response, the next turn asking the owner where to stream must
    /// never be handed the abandoned one. Writing there is rejected event by
    /// event, which is how a real session lost an entire answer.
    #[test]
    fn the_turn_after_an_abandoned_response_streams_into_a_fresh_one() {
        let (emitter, mut rx) = emitter();
        emitter.typing_status_changed(true);
        let abandoned = emitter.stream_start(Some("test-model"));
        emitter.typing_status_changed(false);

        emitter.typing_status_changed(true);
        let next = emitter.ensure_open_response(Some("test-model"));
        assert_ne!(
            next, abandoned,
            "the owner handed back a response it had already thrown away"
        );
        emitter.stream_delta(&next, "the answer");
        emitter.stream_end(next, StreamEndPayload::default());
        emitter.typing_status_changed(false);

        let reports = reports(&mut rx);
        assert!(
            reports
                .iter()
                .all(|report| !report.contains("[response_delta]")),
            "the next turn's content was rejected instead of reaching the user: {reports:?}"
        );
    }

    #[test]
    fn a_closed_response_is_never_handed_back_for_more_content() {
        let (emitter, _rx) = emitter();
        emitter.typing_status_changed(true);
        let closed = emitter.stream_start(Some("test-model"));
        emitter.stream_end(closed.clone(), StreamEndPayload::default());
        assert_ne!(
            emitter.ensure_open_response(Some("test-model")),
            closed,
            "a closed response rejects every further delta"
        );
    }

    /// A caller about to *end* a response must not be able to mint one, or a
    /// turn with nothing to say publishes an empty message to the user.
    #[test]
    fn asking_for_the_open_response_never_opens_one() {
        let (emitter, mut rx) = emitter();
        emitter.typing_status_changed(true);
        assert!(emitter.open_response().is_none());
        emitter.typing_status_changed(false);
        assert!(
            reports(&mut rx).is_empty(),
            "asking for the open response opened one, then abandoned it"
        );
    }

    /// The shape a real Codex session produced: a turn abandoned a response,
    /// the backend kept its handle, and the next turn's whole answer arrived
    /// through it. Every event is still counted and the one-off entries stay
    /// legible instead of being buried under the repeats.
    #[test]
    fn a_wedged_response_reports_every_event_without_repeating_itself() {
        let (emitter, mut rx) = emitter();
        emitter.typing_status_changed(true);
        let stale = emitter.stream_start(Some("test-model"));
        emitter.typing_status_changed(false);
        let _ = reports(&mut rx);

        emitter.typing_status_changed(true);
        for _ in 0..71 {
            emitter.stream_delta(&stale, "x");
        }
        emitter.stream_end(stale, StreamEndPayload::default());
        emitter.typing_status_changed(false);

        let reports = reports(&mut rx);
        assert_eq!(reports.len(), 1, "expected one report, got {reports:?}");
        let report = &reports[0];
        assert!(
            report.starts_with("The backend sent 72 malformed event(s)"),
            "the total must survive collapsing: {report}"
        );
        assert!(
            report.contains("(x71)"),
            "the repeated delta rejection must carry its count: {report}"
        );
        assert!(
            report.contains("[response_end]"),
            "the single end rejection must not be buried: {report}"
        );
        assert_eq!(
            report.lines().count(),
            3,
            "expected a header and two distinct entries: {report}"
        );
    }

    #[test]
    fn permissive_abandoned_response_emits_stream_end_instead_of_discarding() {
        let (emitter, mut rx) = emitter_with_policy(EmitterPolicy::Permissive);
        emitter.typing_status_changed(true);
        let response = emitter.stream_start(Some("test-model"));
        emitter.stream_delta(&response, "important answer text");
        emitter.stream_reasoning_delta(&response, "thought trace");
        emitter.typing_status_changed(false);

        let events = drain_events(&mut rx);
        let reports = violation_reports(&events);
        assert!(
            reports.is_empty(),
            "expected no error reports, got {reports:?}"
        );

        // StreamEnd should have been automatically emitted with the content & reasoning
        let mut saw_stream_end = false;
        let mut final_content = String::new();
        let mut final_reasoning = String::new();
        for event in &events {
            if event.get("kind").and_then(Value::as_str) == Some("StreamEnd") {
                saw_stream_end = true;
                if let Some(content) = event
                    .pointer("/data/message/content")
                    .and_then(Value::as_str)
                {
                    final_content = content.to_owned();
                }
                if let Some(reasoning) = event
                    .pointer("/data/message/reasoning/text")
                    .and_then(Value::as_str)
                {
                    final_reasoning = reasoning.to_owned();
                }
            }
        }
        assert!(
            saw_stream_end,
            "expected StreamEnd to be automatically emitted"
        );
        assert_eq!(final_content, "important answer text");
        assert_eq!(final_reasoning, "thought trace");
    }

    #[test]
    fn permissive_overlapping_response_auto_ends_prior_response() {
        let (emitter, mut rx) = emitter_with_policy(EmitterPolicy::Permissive);
        emitter.typing_status_changed(true);
        let first = emitter.stream_start(Some("model-1"));
        emitter.stream_delta(&first, "part one");

        let second = emitter.stream_start(Some("model-2"));
        emitter.stream_delta(&second, "part two");
        emitter.stream_end(second, StreamEndPayload::default());
        emitter.typing_status_changed(false);

        let events = drain_events(&mut rx);
        let reports = violation_reports(&events);
        assert!(reports.is_empty());

        let mut ends = Vec::new();
        for event in &events {
            if event.get("kind").and_then(Value::as_str) == Some("StreamEnd")
                && let Some(content) = event
                    .pointer("/data/message/content")
                    .and_then(Value::as_str)
            {
                ends.push(content.to_owned());
            }
        }
        assert_eq!(ends, vec!["part one", "part two"]);
    }

    #[test]
    fn permissive_unannounced_delta_auto_opens_response() {
        let (emitter, mut rx) = emitter_with_policy(EmitterPolicy::Permissive);
        emitter.typing_status_changed(true);
        let unannounced = ResponseHandle {
            token: "ghost-token".to_string(),
            message_id: ChatMessageId("ghost-msg".to_string()),
        };
        emitter.stream_delta(&unannounced, "hello unannounced");
        emitter.stream_end(unannounced, StreamEndPayload::default());
        emitter.typing_status_changed(false);

        let events = drain_events(&mut rx);
        assert!(violation_reports(&events).is_empty());

        let mut deltas = Vec::new();
        let mut ended = false;
        for event in &events {
            match event.get("kind").and_then(Value::as_str) {
                Some("StreamDelta") => {
                    if let Some(text) = event.pointer("/data/text").and_then(Value::as_str) {
                        deltas.push(text.to_owned());
                    }
                }
                Some("StreamEnd") => {
                    ended = true;
                }
                _ => {}
            }
        }
        assert_eq!(deltas, vec!["hello unannounced"]);
        assert!(ended);
    }

    #[test]
    fn permissive_undeclared_tool_request_is_auto_declared() {
        let (emitter, mut rx) = emitter_with_policy(EmitterPolicy::Permissive);
        emitter.typing_status_changed(true);
        let ok = emitter.tool_request(
            "call_123",
            ToolRequestType::RunCommand {
                command: "echo test".to_string(),
                working_directory: ".".to_string(),
            },
        );
        assert!(ok, "permissive mode should accept undeclared tool requests");
        emitter.tool_completed(
            "call_123",
            ToolExecutionOutcome::Succeeded {
                result: protocol::ToolExecutionResult::RunCommand {
                    exit_code: 0,
                    stdout: "test".to_string(),
                    stderr: String::new(),
                },
            },
        );
        emitter.typing_status_changed(false);

        let events = drain_events(&mut rx);
        assert!(violation_reports(&events).is_empty());

        let mut kinds = Vec::new();
        for event in &events {
            if let Some(kind) = event.get("kind").and_then(Value::as_str) {
                kinds.push(kind.to_owned());
            }
        }
        assert!(kinds.contains(&"ToolRequest".to_string()));
        assert!(kinds.contains(&"ToolExecutionCompleted".to_string()));
    }

    #[test]
    fn permissive_unexpected_tool_completion_synthesizes_request() {
        let (emitter, mut rx) = emitter_with_policy(EmitterPolicy::Permissive);
        emitter.typing_status_changed(true);
        emitter.tool_completed(
            "call_out_of_blue",
            ToolExecutionOutcome::Cancelled {
                message: "done".to_string(),
            },
        );
        emitter.typing_status_changed(false);

        let events = drain_events(&mut rx);
        assert!(violation_reports(&events).is_empty());

        let mut completed = false;
        for event in &events {
            if event.get("kind").and_then(Value::as_str) == Some("ToolExecutionCompleted") {
                completed = true;
            }
        }
        assert!(completed, "expected completion to be emitted");
    }
}
