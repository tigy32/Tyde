//! Shared turn-event emitter enforcing the chat protocol's ordering
//! invariants. Lifted from tycode-core's `TurnProtocol`
//! (`tycode-core/src/chat/protocol.rs`) and adapted to Tyde's
//! `mpsc::UnboundedSender<Value>` wire format.
//!
//! Each TurnEmitter-backed backend owns exactly one `Arc<TurnEmitter>`
//! for the lifetime of the backend and routes every wire event through
//! its typed methods. There is no `send_raw` escape
//! hatch: the only way to emit is via a method defined here, so the
//! cancellation ordering spec documented on `ChatEvent` is
//! structurally enforceable — if the code compiles, each backend's
//! cancel path fires `StreamEnd` → pending `ToolExecutionCompleted`s →
//! `OperationCancelled` → `TypingStatusChanged(false)` in that order.
//!
//! Invariants:
//!   - `stream_start_with_id` opens a stream. Deltas and ends must carry that
//!     exact typed message id; foreign or missing ids are reported instead of
//!     closing, synthesizing, or rebinding a stream. The string-only methods
//!     are a temporary adapter compatibility seam, not the canonical API.
//!   - `tool_request` records the id; `tool_completed` clears it. Any
//!     still-pending id at cancel time is completed as a cancellation
//!     inside `operation_cancelled`.
//!   - `operation_cancelled` walks the full cancel sequence in one
//!     place, once, per call. The state is reset after so the emitter
//!     can continue serving subsequent turns on the same backend.
//!
//! Non-`ChatEvent` wire kinds (`Settings`, `SessionStarted`,
//! `SessionsList`, `ProfilesList`, `ModuleSchemas`, `ModelsList`,
//! `ConversationCleared`, `ModelRequestTokenUsage`, backend `Error`) also
//! have typed methods so no backend needs to reach past the emitter.

use std::collections::{HashMap, HashSet};

use indexmap::IndexMap;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use protocol::types::StreamIdentityViolation;
use protocol::{
    ChatMessageId, ModelRequestTokenUsage, ServerGeneratedChatMessageIdentity, TaskList,
    TokenUsageUnavailableReason, ToolExecutionNormalizationFailure,
};

use super::agent_control_progress::{
    await_progress_data_for_tool, spawn_progress_data_for_tool_result, tyde_tool_request_type,
    tyde_tool_result,
};

/// Sender half of the wire channel. Kept private to this module; every
/// emission must go through the typed methods below.
pub struct TurnEmitter {
    inner: std::sync::Mutex<TurnEmitterState>,
}

struct TurnEmitterState {
    tx: mpsc::UnboundedSender<Value>,
    default_agent: String,
    default_model: Option<String>,
    stream_open: bool,
    assistant_turn_open: bool,
    current_stream_message_id: Option<ChatMessageId>,
    synthetic_tool_container_id: Option<ChatMessageId>,
    synthetic_tool_call_ids: Vec<String>,
    terminal_stream_message_ids: HashSet<ChatMessageId>,
    identity_violation_reported: bool,
    emitted_tool_requests: IndexMap<String, EmittedToolRequest>,
    detached_tool_requests: IndexMap<String, EmittedToolRequest>,
    completed_tool_requests: HashSet<String>,
    normalization_failures: HashMap<String, PendingToolNormalizationFailure>,
}

#[derive(Clone)]
struct PendingToolNormalizationFailure {
    kind: ToolExecutionNormalizationFailure,
    detail: String,
}

struct EmittedToolRequest {
    name: String,
    arguments: Value,
}

// =============================================================================
// Typed payload structs. Field names mirror the serialized wire shape so the
// emission methods can serialize without going through intermediate helpers.
// =============================================================================

/// Agent-sender identity on `MessageAdded`. The wire serializes this as
/// `{"Assistant": {"agent": "<name>"}}`. Backends usually pass a static
/// `CLAUDE_AGENT_NAME`/`CODEX_AGENT_NAME`/etc. string.
#[derive(Clone, Copy)]
pub struct AgentName<'a>(pub &'a str);

#[derive(Default)]
pub struct StreamEndPayload<'a> {
    pub content: String,
    pub agent: Option<AgentName<'a>>,
    pub model: Option<String>,
    pub request_usage: Option<Value>,
    pub turn_usage: Option<Value>,
    pub cumulative_usage: Option<Value>,
    pub token_usage_unavailable_reason: Option<TokenUsageUnavailableReason>,
    pub reasoning: Option<String>,
    pub tool_calls: Vec<Value>,
    pub context_breakdown: Option<Value>,
    pub images: Vec<protocol::ImageData>,
}

pub struct ToolCompletedPayload<'a> {
    pub tool_call_id: &'a str,
    pub tool_name: &'a str,
    pub tool_result: Value,
    pub success: bool,
    pub error: Option<&'a str>,
}

pub struct AssistantMessagePayload<'a> {
    pub agent: AgentName<'a>,
    pub message_id: Option<&'a str>,
    pub content: String,
    pub reasoning: Option<Value>,
    pub tool_calls: Vec<Value>,
    pub model_info: Option<Value>,
    pub request_usage: Option<Value>,
    pub turn_usage: Option<Value>,
    pub cumulative_usage: Option<Value>,
    pub context_breakdown: Option<Value>,
    pub images: Vec<Value>,
}

pub struct MessageMetadataUpdatePayload {
    pub message_id: String,
    pub model_info: Option<Value>,
    pub request_usage: Option<Value>,
    pub turn_usage: Option<Value>,
    pub cumulative_usage: Option<Value>,
    pub context_breakdown: Option<Value>,
}

pub struct RetryAttemptPayload<'a> {
    pub attempt: u64,
    pub max_retries: u64,
    pub error: &'a str,
    pub backoff_ms: u64,
}

// =============================================================================
// Public API. All methods take `&self` — the emitter is shared across turn
// tasks via `Arc<TurnEmitter>`.
// =============================================================================

impl TurnEmitter {
    pub fn new(tx: mpsc::UnboundedSender<Value>) -> Self {
        Self::new_for_agent(tx, AgentName("assistant"))
    }

    pub fn new_for_agent(tx: mpsc::UnboundedSender<Value>, agent: AgentName<'_>) -> Self {
        Self {
            inner: std::sync::Mutex::new(TurnEmitterState {
                tx,
                default_agent: agent.0.to_string(),
                default_model: None,
                stream_open: false,
                assistant_turn_open: false,
                current_stream_message_id: None,
                synthetic_tool_container_id: None,
                synthetic_tool_call_ids: Vec::new(),
                terminal_stream_message_ids: HashSet::new(),
                identity_violation_reported: false,
                emitted_tool_requests: IndexMap::new(),
                detached_tool_requests: IndexMap::new(),
                completed_tool_requests: HashSet::new(),
                normalization_failures: HashMap::new(),
            }),
        }
    }

    // ---------- Chat stream pairing (protocol-ordered) ----------

    /// Transitional backend-adapter compatibility entry point. New code must
    /// use `stream_start_with_id`.
    pub(crate) fn stream_start(&self, message_id: &str, agent: AgentName<'_>, model: Option<&str>) {
        self.lock().stream_start_legacy(message_id, agent, model);
    }

    pub fn stream_start_with_id(
        &self,
        message_id: ChatMessageId,
        agent: AgentName<'_>,
        model: Option<&str>,
    ) {
        self.lock().stream_start(message_id, agent, model);
    }

    /// Opens a stream under a persisted server-generated identity contract.
    /// Callers retain the contract through replay and use its `ChatMessageId`
    /// for all following typed stream operations.
    pub fn stream_start_with_generated_identity(
        &self,
        identity: &ServerGeneratedChatMessageIdentity,
        agent: AgentName<'_>,
        model: Option<&str>,
    ) {
        self.stream_start_with_id(identity.message_id(), agent, model);
    }

    /// Transitional backend-adapter compatibility entry point. New code must
    /// use `stream_delta_with_id`.
    pub(crate) fn stream_delta(&self, message_id: &str, text: &str) {
        if text.is_empty() {
            return;
        }
        self.lock().stream_delta_legacy(message_id, text);
    }

    pub fn stream_delta_with_id(&self, message_id: ChatMessageId, text: &str) {
        if text.is_empty() {
            return;
        }
        self.lock().stream_delta(message_id, text);
    }

    /// Transitional backend-adapter compatibility entry point. New code must
    /// use `stream_reasoning_delta_with_id`.
    pub(crate) fn stream_reasoning_delta(&self, message_id: &str, text: &str) {
        if text.is_empty() {
            return;
        }
        self.lock().stream_reasoning_delta_legacy(message_id, text);
    }

    pub fn stream_reasoning_delta_with_id(&self, message_id: ChatMessageId, text: &str) {
        if text.is_empty() {
            return;
        }
        self.lock().stream_reasoning_delta(message_id, text);
    }

    /// Transitional backend-adapter compatibility entry point. New code must
    /// carry its intended `ChatMessageId` through `stream_end_with_id`.
    pub(crate) fn stream_end(&self, payload: StreamEndPayload<'_>) {
        self.lock().stream_end_legacy(payload);
    }

    pub fn stream_end_with_id(&self, message_id: ChatMessageId, payload: StreamEndPayload<'_>) {
        self.lock().stream_end(message_id, payload);
    }

    /// Discards an invalid open stream without fabricating a terminal
    /// assistant message. The emitted error/cancellation/idle tail is
    /// intentionally distinct from `operation_cancelled`, which closes a
    /// valid stream with `StreamEnd` first.
    pub fn discard_open_stream_with_identity_violation(&self, violation: StreamIdentityViolation) {
        self.lock()
            .discard_open_stream_with_identity_violation(violation);
    }

    /// Rejects a provider identity reserved before any Tyde stream was
    /// published. The failure remains visible, but there is no stream to close.
    pub fn reject_reserved_stream_with_identity_violation(
        &self,
        violation: StreamIdentityViolation,
    ) {
        self.lock()
            .reject_reserved_stream_with_identity_violation(violation);
    }

    // ---------- Tool pairing (protocol-ordered) ----------

    pub fn tool_request(&self, tool_call_id: &str, tool_name: &str, tool_type: Value) {
        let _ = self
            .lock()
            .tool_request(tool_call_id, tool_name, tool_type, None, false);
    }

    pub fn tool_request_in_container(
        &self,
        tool_call_id: &str,
        tool_name: &str,
        tool_type: Value,
    ) -> Option<ChatMessageId> {
        self.lock()
            .tool_request(tool_call_id, tool_name, tool_type, None, true)
    }

    pub fn tool_request_with_normalization_failure(
        &self,
        tool_call_id: &str,
        tool_name: &str,
        tool_type: Value,
        normalization_failure: ToolExecutionNormalizationFailure,
    ) {
        let _ = self.lock().tool_request(
            tool_call_id,
            tool_name,
            tool_type,
            Some(normalization_failure),
            false,
        );
    }

    pub fn tool_request_in_container_with_normalization_failure(
        &self,
        tool_call_id: &str,
        tool_name: &str,
        tool_type: Value,
        normalization_failure: ToolExecutionNormalizationFailure,
    ) -> Option<ChatMessageId> {
        self.lock().tool_request(
            tool_call_id,
            tool_name,
            tool_type,
            Some(normalization_failure),
            true,
        )
    }

    pub fn close_tool_container(&self, message_id: ChatMessageId) {
        self.lock().close_tool_container(message_id);
    }

    pub fn close_tool_container_with_images(
        &self,
        message_id: ChatMessageId,
        images: Vec<protocol::ImageData>,
    ) {
        self.lock()
            .close_tool_container_with_images(message_id, images);
    }

    pub fn tool_call_declarations(&self, tool_call_ids: &[String]) -> Vec<Value> {
        let state = self.lock();
        tool_call_ids
            .iter()
            .filter_map(|tool_call_id| {
                state
                    .emitted_tool_requests
                    .get(tool_call_id)
                    .map(|request| {
                        json!({
                            "id": tool_call_id,
                            "name": request.name.clone(),
                            "arguments": request.arguments.clone(),
                        })
                    })
            })
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn has_open_stream(&self) -> bool {
        self.lock().stream_open
    }

    pub fn tool_completed(&self, data: ToolCompletedPayload<'_>) -> Option<ChatMessageId> {
        self.lock().tool_completed(data, None)
    }

    /// Move a tool out of turn ownership while retaining its request identity.
    /// Detached tools survive later turn resets/cancellation and are completed
    /// by their own out-of-band terminal notification.
    pub fn detach_tool(&self, tool_call_id: &str) -> bool {
        let mut state = self.lock();
        let Some(request) = state.emitted_tool_requests.shift_remove(tool_call_id) else {
            return state.detached_tool_requests.contains_key(tool_call_id);
        };
        state
            .detached_tool_requests
            .insert(tool_call_id.to_owned(), request);
        true
    }

    pub(crate) fn has_pending_tool_request(&self, tool_call_id: &str) -> bool {
        self.lock().is_tool_pending(tool_call_id)
    }

    pub fn tool_completed_with_normalization_failure(
        &self,
        data: ToolCompletedPayload<'_>,
        normalization_failure: ToolExecutionNormalizationFailure,
    ) -> Option<ChatMessageId> {
        self.lock()
            .tool_completed(data, Some(normalization_failure))
    }

    pub fn fail_pending_tool(&self, tool_call_id: &str, error: &str) -> bool {
        let mut state = self.lock();
        let Some(tool_name) = state.pending_tool_name(tool_call_id).cloned() else {
            return false;
        };
        state.send_tool_completed(
            tool_call_id,
            &tool_name,
            failed_tool_result("Tool execution failed", error),
            false,
            Some(error),
        );
        true
    }

    /// Live progress snapshot for a tool call. Deliberately stateless:
    /// background tasks (workflows, sub-agents) keep emitting progress
    /// after their tool call completes and across turn boundaries, when
    /// the per-turn pending/completed maps have already been reset — so
    /// no pairing is enforced beyond a non-empty id. The frontend
    /// matches by `tool_call_id` against its persistent index.
    pub fn tool_progress(&self, data: &protocol::ToolProgressData) {
        if data.tool_call_id.is_empty() {
            tracing::debug!(
                "dropping ToolProgress for tool '{}' with empty tool_call_id",
                data.tool_name
            );
            return;
        }
        self.lock().send_tool_progress(data);
    }

    // ---------- Cancellation & lifecycle ----------

    /// Full cancellation path: close open stream → complete pending
    /// tools → `OperationCancelled` → `TypingStatusChanged(false)`.
    /// Resets per-turn state so the emitter can serve the next turn.
    pub fn operation_cancelled(&self, message: &str) {
        self.lock().abort(message);
    }

    // ---------- Messages (user / assistant / system / error / warning) ----------

    pub fn user_message(&self, content: &str, images: Vec<Value>) {
        let mut state = self.lock();
        state.assistant_turn_open = false;
        state.send(json!({
            "kind": "MessageAdded",
            "data": {
                "message_id": Value::Null,
                "timestamp": now_ms(),
                "sender": "User",
                "content": content,
                "reasoning": Value::Null,
                "tool_calls": [],
                "model_info": Value::Null,
                "token_usage": Value::Null,
                "context_breakdown": Value::Null,
                "images": images,
            },
        }));
    }

    pub fn system_message(&self, content: &str) {
        let mut state = self.lock();
        state.assistant_turn_open = false;
        state.send(json!({
            "kind": "MessageAdded",
            "data": {
                "message_id": Value::Null,
                "timestamp": now_ms(),
                "sender": "System",
                "content": content,
                "reasoning": Value::Null,
                "tool_calls": [],
                "model_info": Value::Null,
                "token_usage": Value::Null,
                "context_breakdown": Value::Null,
                "images": [],
            },
        }));
    }

    pub fn warning_message(&self, content: &str) {
        let mut state = self.lock();
        state.assistant_turn_open = false;
        state.send(json!({
            "kind": "MessageAdded",
            "data": {
                "message_id": Value::Null,
                "timestamp": now_ms(),
                "sender": "Warning",
                "content": content,
                "reasoning": Value::Null,
                "tool_calls": [],
                "model_info": Value::Null,
                "token_usage": Value::Null,
                "context_breakdown": Value::Null,
                "images": [],
            },
        }));
    }

    pub fn error_message(&self, content: &str) {
        let mut state = self.lock();
        state.assistant_turn_open = false;
        state.send(json!({
            "kind": "MessageAdded",
            "data": {
                "message_id": Value::Null,
                "timestamp": now_ms(),
                "sender": "Error",
                "content": content,
                "reasoning": Value::Null,
                "tool_calls": [],
                "model_info": Value::Null,
                "token_usage": Value::Null,
                "context_breakdown": Value::Null,
                "images": [],
            },
        }));
    }

    pub fn assistant_message(&self, payload: AssistantMessagePayload<'_>) {
        self.lock().assistant_message(payload);
    }

    pub fn message_metadata_updated(&self, payload: MessageMetadataUpdatePayload) {
        self.lock().message_metadata_updated(payload);
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

    // ---------- Misc chat events ----------

    pub fn typing_status_changed(&self, typing: bool) {
        let mut state = self.lock();
        if !typing {
            state.complete_pending_normalization_failures();
        }
        state.send(json!({
            "kind": "TypingStatusChanged",
            "data": typing,
        }));
    }

    pub fn task_update(&self, tasks: &TaskList) {
        let value = serde_json::to_value(tasks).unwrap_or(Value::Null);
        self.lock().send(json!({
            "kind": "TaskUpdate",
            "data": value,
        }));
    }

    pub fn retry_attempt(&self, payload: RetryAttemptPayload<'_>) {
        self.lock().send(json!({
            "kind": "RetryAttempt",
            "data": {
                "attempt": payload.attempt,
                "max_retries": payload.max_retries,
                "error": payload.error,
                "backoff_ms": payload.backoff_ms,
            },
        }));
    }

    // ---------- Non-ChatEvent wire kinds (control / discovery / out-of-band) ----------

    /// Signals that the backend opened (or resumed) an upstream session
    /// and knows the id. Consumed by each backend's event forwarder to
    /// populate its `session_id` sink; never reaches the agent actor as
    /// a `ChatEvent`.
    pub fn session_started(&self, session_id: &str) {
        self.lock().send(json!({
            "kind": "SessionStarted",
            "data": { "session_id": session_id },
        }));
    }

    /// Backend-level error event. The forwarder lifts this into a
    /// `MessageAdded { sender: Error }` `ChatEvent` for the client.
    pub fn backend_error(&self, message: &str) {
        self.lock().send(json!({
            "kind": "Error",
            "data": message,
        }));
    }

    pub fn conversation_cleared(&self) {
        let mut state = self.lock();
        state.reset_turn_state();
        state.detached_tool_requests.clear();
        state.terminal_stream_message_ids.clear();
        state.identity_violation_reported = false;
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

    /// A line of stderr from the backend subprocess.
    pub fn subprocess_stderr(&self, line: &str) {
        self.lock().send(json!({
            "kind": "SubprocessStderr",
            "data": line,
        }));
    }

    /// The backend subprocess has exited.
    pub fn subprocess_exit(&self, exit_code: Option<i32>) {
        self.lock().send(json!({
            "kind": "SubprocessExit",
            "data": { "exit_code": exit_code },
        }));
    }

    // ---------- Introspection ----------

    pub fn is_stream_open(&self) -> bool {
        self.lock().stream_open
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, TurnEmitterState> {
        self.inner.lock().expect("TurnEmitter mutex poisoned")
    }
}

impl TurnEmitterState {
    fn send(&self, event: Value) {
        // Failure means the receiver is gone; the event stream is over.
        // The caller does not depend on delivery, so we drop silently.
        let _ = self.tx.send(event);
    }

    fn stream_start_legacy(&mut self, message_id: &str, agent: AgentName<'_>, model: Option<&str>) {
        let message_id = match required_message_id(message_id) {
            Some(message_id) => message_id,
            None => {
                self.stream_identity_violation(StreamIdentityViolation::MissingMessageId);
                return;
            }
        };
        self.stream_start(message_id, agent, model);
    }

    fn stream_start(
        &mut self,
        message_id: ChatMessageId,
        agent: AgentName<'_>,
        model: Option<&str>,
    ) {
        if self.terminal_stream_message_ids.contains(&message_id) {
            self.stream_identity_violation(StreamIdentityViolation::DuplicateTerminalMessageId);
            return;
        }
        if self.stream_open {
            self.stream_identity_violation(StreamIdentityViolation::ForeignActiveMessageId);
            return;
        }
        self.identity_violation_reported = false;
        self.stream_open = true;
        self.assistant_turn_open = true;
        self.current_stream_message_id = Some(message_id.clone());
        self.default_agent = agent.0.to_string();
        self.default_model = model.map(str::to_owned);
        let model_value = model
            .map(|m| Value::String(m.to_owned()))
            .unwrap_or(Value::Null);
        self.send(json!({
            "kind": "StreamStart",
            "data": {
                "message_id": message_id.0,
                "agent": agent.0,
                "model": model_value,
            },
        }));
    }

    fn stream_delta_legacy(&mut self, message_id: &str, text: &str) {
        let Some(message_id) = required_message_id(message_id) else {
            self.stream_identity_violation(StreamIdentityViolation::MissingMessageId);
            return;
        };
        self.stream_delta(message_id, text);
    }

    fn stream_delta(&mut self, message_id: ChatMessageId, text: &str) {
        let Some(message_id) = self.active_stream_message_id(message_id) else {
            return;
        };
        self.send(json!({
            "kind": "StreamDelta",
            "data": { "message_id": message_id.0, "text": text },
        }));
    }

    fn stream_reasoning_delta_legacy(&mut self, message_id: &str, text: &str) {
        let Some(message_id) = required_message_id(message_id) else {
            self.stream_identity_violation(StreamIdentityViolation::MissingMessageId);
            return;
        };
        self.stream_reasoning_delta(message_id, text);
    }

    fn stream_reasoning_delta(&mut self, message_id: ChatMessageId, text: &str) {
        let Some(message_id) = self.active_stream_message_id(message_id) else {
            return;
        };
        self.send(json!({
            "kind": "StreamReasoningDelta",
            "data": { "message_id": message_id.0, "text": text },
        }));
    }

    fn stream_end_legacy(&mut self, payload: StreamEndPayload<'_>) {
        let Some(message_id) = self.current_stream_message_id.clone() else {
            self.stream_identity_violation(StreamIdentityViolation::MissingMessageId);
            return;
        };
        self.stream_end(message_id, payload);
    }

    fn stream_end(&mut self, message_id: ChatMessageId, payload: StreamEndPayload<'_>) {
        if !self.stream_open || self.current_stream_message_id.as_ref() != Some(&message_id) {
            self.stream_identity_violation(StreamIdentityViolation::MismatchedEndMessageId);
            return;
        }
        self.stream_open = false;
        self.current_stream_message_id = None;
        if self.synthetic_tool_container_id.as_ref() == Some(&message_id) {
            self.synthetic_tool_container_id = None;
        }
        self.terminal_stream_message_ids.insert(message_id.clone());
        self.send(build_stream_end_value(
            &payload,
            &self.default_agent,
            &message_id.0,
        ));
    }

    fn assistant_message(&mut self, payload: AssistantMessagePayload<'_>) {
        self.assistant_turn_open = true;
        self.identity_violation_reported = false;
        self.send(build_assistant_message_value(&payload));
    }

    fn message_metadata_updated(&mut self, payload: MessageMetadataUpdatePayload) {
        let message_id = payload.message_id.trim().to_string();
        if message_id.is_empty() {
            return;
        }
        if payload.model_info.is_none()
            && payload.request_usage.is_none()
            && payload.turn_usage.is_none()
            && payload.cumulative_usage.is_none()
            && payload.context_breakdown.is_none()
        {
            return;
        }
        let token_usage = build_message_token_usage_value(
            payload.request_usage,
            payload.turn_usage,
            payload.cumulative_usage,
            None,
        );
        self.send(json!({
            "kind": "MessageMetadataUpdated",
            "data": {
                "message_id": message_id,
                "model_info": payload.model_info.unwrap_or(Value::Null),
                "token_usage": token_usage,
                "context_breakdown": payload.context_breakdown.unwrap_or(Value::Null),
            },
        }));
    }

    fn tool_request(
        &mut self,
        tool_call_id: &str,
        tool_name: &str,
        provider_tool_type: Value,
        normalization_failure: Option<ToolExecutionNormalizationFailure>,
        own_container: bool,
    ) -> Option<ChatMessageId> {
        let mut normalization_failure =
            normalization_failure.map(|kind| PendingToolNormalizationFailure {
                kind,
                detail: normalization_failure_detail(kind),
            });
        let tool_type = match tyde_tool_request_type(tool_name, &provider_tool_type) {
            Ok(Some(typed)) => serde_json::to_value(typed).expect("serialize tool request"),
            Ok(None) => provider_tool_type,
            Err(error) => {
                tracing::error!(
                    tool = tool_name,
                    tool_call_id,
                    detail = %error.detail,
                    "Canonical Tyde tool request normalization failed"
                );
                normalization_failure = merge_normalization_failures(
                    normalization_failure,
                    Some(PendingToolNormalizationFailure {
                        kind: error.normalization_failure,
                        detail: error.to_string(),
                    }),
                );
                provider_tool_type
            }
        };
        if let Some(request) = self
            .emitted_tool_requests
            .get(tool_call_id)
            .or_else(|| self.detached_tool_requests.get(tool_call_id))
            && (self.completed_tool_requests.contains(tool_call_id)
                || (request.name == tool_name && request.arguments == tool_type))
        {
            tracing::debug!(
                tool_call_id,
                tool_name,
                completed = self.completed_tool_requests.contains(tool_call_id),
                same_request = request.name == tool_name && request.arguments == tool_type,
                "Ignoring duplicate tool request"
            );
            return None;
        }
        let opened_container = if own_container || !self.assistant_turn_open {
            self.ensure_assistant_turn_open(tool_call_id)
        } else {
            None
        };
        if self.is_tool_pending(tool_call_id) {
            let existing_name = self
                .emitted_tool_requests
                .get(tool_call_id)
                .map(|request| request.name.clone())
                .unwrap_or_else(|| tool_name.to_string());
            self.send_tool_completed(
                tool_call_id,
                &existing_name,
                cancelled_tool_result("Duplicate tool request was superseded"),
                false,
                Some("Duplicate tool request was superseded"),
            );
        }
        self.completed_tool_requests.remove(tool_call_id);
        let normalization_failed = normalization_failure.is_some();
        match normalization_failure {
            Some(failure) => {
                self.normalization_failures
                    .insert(tool_call_id.to_string(), failure);
            }
            None => {
                self.normalization_failures.remove(tool_call_id);
            }
        }
        self.emitted_tool_requests.insert(
            tool_call_id.to_string(),
            EmittedToolRequest {
                name: tool_name.to_string(),
                arguments: tool_type.clone(),
            },
        );
        if self.synthetic_tool_container_id.is_some()
            && !self
                .synthetic_tool_call_ids
                .iter()
                .any(|id| id == tool_call_id)
        {
            self.synthetic_tool_call_ids.push(tool_call_id.to_string());
        }
        self.send(json!({
            "kind": "ToolRequest",
            "data": {
                "tool_call_id": tool_call_id,
                "tool_name": tool_name,
                "tool_type": tool_type,
            },
        }));
        if !normalization_failed
            && let Some(progress) =
                await_progress_data_for_tool(tool_call_id, tool_name, &tool_type)
        {
            self.send_tool_progress(&progress);
        }
        opened_container
    }

    fn tool_completed(
        &mut self,
        mut data: ToolCompletedPayload<'_>,
        normalization_failure: Option<ToolExecutionNormalizationFailure>,
    ) -> Option<ChatMessageId> {
        let mut normalization_failure =
            normalization_failure.map(|kind| PendingToolNormalizationFailure {
                kind,
                detail: normalization_failure_detail(kind),
            });
        if self.completed_tool_requests.contains(data.tool_call_id) {
            tracing::debug!(
                tool_call_id = data.tool_call_id,
                tool_name = data.tool_name,
                "Ignoring duplicate terminal tool completion"
            );
            return None;
        }
        let mut spawn_progress = None;
        if data.success {
            match tyde_tool_result(data.tool_name, &data.tool_result) {
                Ok(Some(typed)) => {
                    data.tool_result = serde_json::to_value(typed).expect("serialize tool result");
                }
                Ok(None) => {}
                Err(error) => {
                    tracing::error!(
                        tool = data.tool_name,
                        tool_call_id = data.tool_call_id,
                        detail = %error.detail,
                        "Canonical Tyde tool result normalization failed"
                    );
                    normalization_failure = merge_normalization_failures(
                        normalization_failure,
                        Some(PendingToolNormalizationFailure {
                            kind: error.normalization_failure,
                            detail: error.to_string(),
                        }),
                    );
                }
            }
            if normalization_failure.is_none() {
                spawn_progress = spawn_progress_data_for_tool_result(
                    data.tool_call_id,
                    data.tool_name,
                    &data.tool_result,
                );
            }
        }
        let mut opened_container = None;
        match self.pending_tool_name(data.tool_call_id).cloned() {
            Some(expected_name) if expected_name != data.tool_name => {
                self.send_tool_completed(
                    data.tool_call_id,
                    &expected_name,
                    cancelled_tool_result("Tool completion name mismatch was superseded"),
                    false,
                    Some("Tool completion name mismatch was superseded"),
                );
                opened_container = self.tool_request(
                    data.tool_call_id,
                    data.tool_name,
                    json!({
                        "kind": "Other",
                        "args": {
                            "synthetic": true,
                            "reason": "tool completion arrived without a matching request",
                        },
                    }),
                    None,
                    true,
                );
            }
            Some(_) => {}
            None => {
                opened_container = self.tool_request(
                    data.tool_call_id,
                    data.tool_name,
                    json!({
                        "kind": "Other",
                        "args": {
                            "synthetic": true,
                            "reason": "tool completion arrived without a pending request",
                        },
                    }),
                    None,
                    true,
                );
            }
        }
        let normalization_failure = merge_normalization_failures(
            self.normalization_failures.remove(data.tool_call_id),
            normalization_failure,
        );
        if let Some(progress) = spawn_progress {
            self.send_tool_progress(&progress);
        }
        let success = normalization_failure.is_none() && data.success;
        let error = normalization_failure
            .as_ref()
            .map(|failure| failure.detail.clone())
            .or_else(|| data.error.map(str::to_owned));
        self.send_tool_completed_with_normalization_failure(
            data.tool_call_id,
            data.tool_name,
            data.tool_result,
            success,
            error.as_deref(),
            normalization_failure,
        );
        opened_container
    }

    fn send_tool_progress(&self, data: &protocol::ToolProgressData) {
        if let Some(request) = self
            .emitted_tool_requests
            .get(&data.tool_call_id)
            .or_else(|| self.detached_tool_requests.get(&data.tool_call_id))
            && request.name != data.tool_name
        {
            tracing::error!(
                tool_call_id = data.tool_call_id,
                request_tool_name = request.name,
                progress_tool_name = data.tool_name,
                "Rejecting tool progress with conflicting tool identity"
            );
            self.send(json!({
                "kind": "Error",
                "data": format!(
                    "Tool progress identity mismatch for '{}': request is '{}', progress is '{}'",
                    data.tool_call_id, request.name, data.tool_name
                ),
            }));
            return;
        }
        let payload = match serde_json::to_value(data) {
            Ok(payload) => payload,
            Err(error) => {
                tracing::warn!("failed to serialize ToolProgressData: {error}");
                return;
            }
        };
        self.send(json!({
            "kind": "ToolProgress",
            "data": payload,
        }));
    }

    fn send_tool_completed(
        &mut self,
        tool_call_id: &str,
        tool_name: &str,
        tool_result: Value,
        success: bool,
        error: Option<&str>,
    ) {
        let normalization_failure = self.normalization_failures.remove(tool_call_id);
        let success = normalization_failure.is_none() && success;
        let error = normalization_failure
            .as_ref()
            .map(|failure| failure.detail.clone())
            .or_else(|| error.map(str::to_owned));
        self.send_tool_completed_with_normalization_failure(
            tool_call_id,
            tool_name,
            tool_result,
            success,
            error.as_deref(),
            normalization_failure,
        );
    }

    fn send_tool_completed_with_normalization_failure(
        &mut self,
        tool_call_id: &str,
        tool_name: &str,
        tool_result: Value,
        success: bool,
        error: Option<&str>,
        normalization_failure: Option<PendingToolNormalizationFailure>,
    ) {
        self.completed_tool_requests
            .insert(tool_call_id.to_string());
        self.detached_tool_requests.shift_remove(tool_call_id);
        let error_value = error
            .map(|s| Value::String(s.to_owned()))
            .unwrap_or(Value::Null);
        let mut data = json!({
            "tool_call_id": tool_call_id,
            "tool_name": tool_name,
            "tool_result": tool_result,
            "success": success,
            "error": error_value,
        });
        if let Some(normalization_failure) = normalization_failure {
            data["normalization_failure"] = serde_json::to_value(normalization_failure.kind)
                .expect("serialize normalization failure");
        }
        self.send(json!({
            "kind": "ToolExecutionCompleted",
            "data": data,
        }));
    }

    fn abort(&mut self, cancellation_message: &str) {
        if let Some(message_id) = self.current_stream_message_id.clone() {
            if self.synthetic_tool_container_id.as_ref() == Some(&message_id) {
                self.close_tool_container(message_id);
            } else {
                self.stream_end(message_id, StreamEndPayload::default());
            }
        }

        self.complete_pending_tools_as_cancelled("Tool execution was cancelled by user");

        self.send(json!({
            "kind": "OperationCancelled",
            "data": { "message": cancellation_message },
        }));
        self.send(json!({
            "kind": "TypingStatusChanged",
            "data": false,
        }));
        self.reset_turn_state();
    }

    fn active_stream_message_id(&mut self, message_id: ChatMessageId) -> Option<ChatMessageId> {
        if !self.stream_open || self.current_stream_message_id.as_ref() != Some(&message_id) {
            self.stream_identity_violation(StreamIdentityViolation::ForeignActiveMessageId);
            return None;
        }
        Some(message_id)
    }

    fn ensure_assistant_turn_open(&mut self, message_id: &str) -> Option<ChatMessageId> {
        if !self.stream_open {
            return self.open_synthetic_stream(message_id);
        }
        None
    }

    fn open_synthetic_stream(&mut self, message_id: &str) -> Option<ChatMessageId> {
        let message_id = required_message_id(message_id)?;
        let agent = self.default_agent.clone();
        let model = self.default_model.clone();
        self.stream_start(message_id.clone(), AgentName(&agent), model.as_deref());
        if self.stream_open && self.current_stream_message_id.as_ref() == Some(&message_id) {
            self.synthetic_tool_container_id = Some(message_id.clone());
            Some(message_id)
        } else {
            None
        }
    }

    fn close_tool_container(&mut self, message_id: ChatMessageId) {
        self.close_tool_container_with_images(message_id, Vec::new());
    }

    fn close_tool_container_with_images(
        &mut self,
        message_id: ChatMessageId,
        images: Vec<protocol::ImageData>,
    ) {
        if self.synthetic_tool_container_id.as_ref() != Some(&message_id)
            || !self.stream_open
            || self.current_stream_message_id.as_ref() != Some(&message_id)
        {
            self.stream_identity_violation(StreamIdentityViolation::MismatchedEndMessageId);
            return;
        }
        let tool_calls = self
            .synthetic_tool_call_ids
            .iter()
            .filter_map(|tool_call_id| {
                self.emitted_tool_requests.get(tool_call_id).map(|request| {
                    json!({
                        "id": tool_call_id,
                        "name": request.name.clone(),
                        "arguments": request.arguments.clone(),
                    })
                })
            })
            .collect();
        self.stream_end(
            message_id,
            StreamEndPayload {
                tool_calls,
                images,
                ..StreamEndPayload::default()
            },
        );
        self.synthetic_tool_container_id = None;
        self.synthetic_tool_call_ids.clear();
        self.assistant_turn_open = false;
    }

    fn stream_identity_violation(&mut self, violation: StreamIdentityViolation) {
        if self.identity_violation_reported {
            return;
        }
        if self.stream_open {
            self.discard_open_stream_with_identity_violation(violation);
            return;
        }
        self.identity_violation_reported = true;
        self.send(json!({
            "kind": "Error",
            "data": stream_identity_violation_message(violation),
        }));
    }

    fn discard_open_stream_with_identity_violation(&mut self, violation: StreamIdentityViolation) {
        if self.identity_violation_reported {
            return;
        }
        self.identity_violation_reported = true;
        if let Some(message_id) = self.current_stream_message_id.take() {
            self.terminal_stream_message_ids.insert(message_id);
        }
        self.reset_turn_state();
        self.send(json!({
            "kind": "Error",
            "data": stream_identity_violation_message(violation),
        }));
        self.send(json!({
            "kind": "OperationCancelled",
            "data": { "message": "Stream identity violation" },
        }));
        self.send(json!({
            "kind": "TypingStatusChanged",
            "data": false,
        }));
    }

    fn reject_reserved_stream_with_identity_violation(
        &mut self,
        violation: StreamIdentityViolation,
    ) {
        if self.identity_violation_reported {
            return;
        }
        if self.stream_open {
            self.discard_open_stream_with_identity_violation(violation);
            return;
        }
        self.identity_violation_reported = true;
        self.reset_turn_state();
        self.send(json!({
            "kind": "Error",
            "data": stream_identity_violation_message(violation),
        }));
        self.send(json!({
            "kind": "OperationCancelled",
            "data": { "message": "Stream identity violation" },
        }));
        self.send(json!({
            "kind": "TypingStatusChanged",
            "data": false,
        }));
    }

    fn complete_pending_tools_as_cancelled(&mut self, detailed_message: &str) {
        let pending: Vec<(String, String)> = self
            .emitted_tool_requests
            .iter()
            .filter(|(id, _)| !self.completed_tool_requests.contains(*id))
            .map(|(id, request)| (id.clone(), request.name.clone()))
            .collect();

        for (tool_call_id, tool_name) in pending {
            self.send_tool_completed(
                &tool_call_id,
                &tool_name,
                cancelled_tool_result(detailed_message),
                false,
                Some("Cancelled"),
            );
        }
    }

    fn complete_pending_normalization_failures(&mut self) {
        let pending = self
            .normalization_failures
            .iter()
            .filter_map(|(tool_call_id, failure)| {
                self.pending_tool_name(tool_call_id).map(|tool_name| {
                    (
                        tool_call_id.clone(),
                        tool_name.clone(),
                        failure.detail.clone(),
                    )
                })
            })
            .collect::<Vec<_>>();
        for (tool_call_id, tool_name, detail) in pending {
            self.send_tool_completed(
                &tool_call_id,
                &tool_name,
                failed_tool_result("Invalid tool request", &detail),
                false,
                None,
            );
        }
    }

    fn pending_tool_name(&self, tool_call_id: &str) -> Option<&String> {
        self.emitted_tool_requests
            .get(tool_call_id)
            .or_else(|| self.detached_tool_requests.get(tool_call_id))
            .map(|request| &request.name)
            .filter(|_| !self.completed_tool_requests.contains(tool_call_id))
    }

    fn is_tool_pending(&self, tool_call_id: &str) -> bool {
        self.pending_tool_name(tool_call_id).is_some()
    }

    fn reset_turn_state(&mut self) {
        self.stream_open = false;
        self.assistant_turn_open = false;
        self.current_stream_message_id = None;
        self.synthetic_tool_container_id = None;
        self.synthetic_tool_call_ids.clear();
        self.emitted_tool_requests.clear();
        self.completed_tool_requests.clear();
        self.normalization_failures.clear();
    }
}

fn merge_normalization_failures(
    existing: Option<PendingToolNormalizationFailure>,
    incoming: Option<PendingToolNormalizationFailure>,
) -> Option<PendingToolNormalizationFailure> {
    match (existing, incoming) {
        (None, None) => None,
        (Some(failure), None) | (None, Some(failure)) => Some(failure),
        (Some(existing), Some(incoming)) => Some(PendingToolNormalizationFailure {
            kind: existing.kind.combined_with(incoming.kind),
            detail: if existing.detail == incoming.detail {
                existing.detail
            } else {
                format!("{}; {}", existing.detail, incoming.detail)
            },
        }),
    }
}

fn normalization_failure_detail(failure: ToolExecutionNormalizationFailure) -> String {
    match failure {
        ToolExecutionNormalizationFailure::CanonicalRequest => {
            "Canonical tool request failed typed validation".to_string()
        }
        ToolExecutionNormalizationFailure::CanonicalResult => {
            "Canonical tool result failed typed validation".to_string()
        }
        ToolExecutionNormalizationFailure::CanonicalRequestAndResult => {
            "Canonical tool request and result failed typed validation".to_string()
        }
    }
}

fn build_stream_end_value(
    payload: &StreamEndPayload<'_>,
    default_agent: &str,
    message_id: &str,
) -> Value {
    let agent_name = payload.agent.map(|a| a.0).unwrap_or(default_agent);
    let model_info = payload
        .model
        .as_deref()
        .filter(|m| !m.trim().is_empty())
        .map(|m| json!({ "model": m }))
        .unwrap_or(Value::Null);
    let usage_value = build_message_token_usage_value(
        payload.request_usage.clone(),
        payload.turn_usage.clone(),
        payload.cumulative_usage.clone(),
        payload.token_usage_unavailable_reason,
    );
    let reasoning_value = payload
        .reasoning
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .map(|text| json!({ "text": text }))
        .unwrap_or(Value::Null);
    let context_breakdown_value = payload.context_breakdown.clone().unwrap_or(Value::Null);

    json!({
        "kind": "StreamEnd",
        "data": {
            "message": {
                "message_id": message_id,
                "timestamp": now_ms(),
                "sender": { "Assistant": { "agent": agent_name } },
                "content": payload.content,
                "reasoning": reasoning_value,
                "tool_calls": payload.tool_calls,
                "model_info": model_info,
                "token_usage": usage_value,
                "context_breakdown": context_breakdown_value,
                "images": payload.images,
            }
        },
    })
}

fn required_message_id(message_id: &str) -> Option<ChatMessageId> {
    (!message_id.trim().is_empty()).then(|| ChatMessageId(message_id.to_owned()))
}

fn stream_identity_violation_message(violation: StreamIdentityViolation) -> &'static str {
    match violation {
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
    }
}

fn cancelled_tool_result(detailed_message: &str) -> Value {
    json!({
        "kind": "Cancelled",
        "message": detailed_message,
    })
}

fn failed_tool_result(short_message: &str, detailed_message: &str) -> Value {
    json!({
        "kind": "Error",
        "short_message": short_message,
        "detailed_message": detailed_message,
    })
}

fn build_assistant_message_value(payload: &AssistantMessagePayload<'_>) -> Value {
    json!({
        "kind": "MessageAdded",
        "data": {
            "message_id": payload.message_id,
            "timestamp": now_ms(),
            "sender": { "Assistant": { "agent": payload.agent.0 } },
            "content": payload.content,
            "reasoning": payload.reasoning.clone().unwrap_or(Value::Null),
            "tool_calls": payload.tool_calls,
            "model_info": payload.model_info.clone().unwrap_or(Value::Null),
            "token_usage": build_message_token_usage_value(
                payload.request_usage.clone(),
                payload.turn_usage.clone(),
                payload.cumulative_usage.clone(),
                None,
            ),
            "context_breakdown": payload.context_breakdown.clone().unwrap_or(Value::Null),
            "images": payload.images,
        },
    })
}

fn build_message_token_usage_value(
    request_usage: Option<Value>,
    turn_usage: Option<Value>,
    cumulative_usage: Option<Value>,
    unavailable_reason: Option<TokenUsageUnavailableReason>,
) -> Value {
    if request_usage.is_none() && turn_usage.is_none() && cumulative_usage.is_none() {
        return unavailable_reason
            .map(|reason| {
                json!({
                    "request": token_usage_unavailable_scope_value(reason),
                    "turn": token_usage_unavailable_scope_value(reason),
                    "cumulative": token_usage_unavailable_scope_value(reason),
                })
            })
            .unwrap_or(Value::Null);
    }
    let unavailable_reason =
        unavailable_reason.unwrap_or(TokenUsageUnavailableReason::BackendDidNotReport);
    json!({
        "request": token_usage_scope_value(request_usage, unavailable_reason),
        "turn": token_usage_scope_value(turn_usage, unavailable_reason),
        "cumulative": token_usage_scope_value(cumulative_usage, unavailable_reason),
    })
}

fn token_usage_scope_value(
    usage: Option<Value>,
    unavailable_reason: TokenUsageUnavailableReason,
) -> Value {
    match usage {
        Some(usage) => json!({ "kind": "known", "usage": usage }),
        None => token_usage_unavailable_scope_value(unavailable_reason),
    }
}

fn token_usage_unavailable_scope_value(reason: TokenUsageUnavailableReason) -> Value {
    json!({
        "kind": "unavailable",
        "reason": token_usage_unavailable_reason_str(reason),
    })
}

fn token_usage_unavailable_reason_str(reason: TokenUsageUnavailableReason) -> &'static str {
    match reason {
        TokenUsageUnavailableReason::BackendDidNotReport => "backend_did_not_report",
        TokenUsageUnavailableReason::ProviderScopeAmbiguous => "provider_scope_ambiguous",
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
    use protocol::{
        AgentBootstrapEvent, AgentBootstrapPayload, AgentId, AgentOrigin, AgentStartPayload,
        BackendKind, BackendSetupPayload, ChatEvent, Envelope, FrameKind, HostBootstrapPayload,
        HostSettings, MobileAccessStatePayload, MobileBrokerStatus, MobilePairingState,
        NewAgentPayload, PROTOCOL_VERSION, ProtocolValidator, ServerGeneratedChatMessageIdOrigin,
        StreamPath, TeamPresetCatalog, Version, WelcomePayload,
    };

    fn recv_events(rx: &mut mpsc::UnboundedReceiver<Value>) -> Vec<Value> {
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        events
    }

    fn recv_kinds(rx: &mut mpsc::UnboundedReceiver<Value>) -> Vec<String> {
        event_kinds(&recv_events(rx))
    }

    fn event_kinds(events: &[Value]) -> Vec<String> {
        events
            .iter()
            .filter_map(|event| event.get("kind").and_then(Value::as_str))
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn normalization_failures_are_carried_on_the_paired_completion() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let emitter = TurnEmitter::new(tx);
        emitter.tool_request_with_normalization_failure(
            "tool-normalization",
            "tyde_send_agent_message",
            json!({ "kind": "Other", "args": { "agent_id": "agent-a" } }),
            ToolExecutionNormalizationFailure::CanonicalRequest,
        );
        emitter.tool_completed_with_normalization_failure(
            ToolCompletedPayload {
                tool_call_id: "tool-normalization",
                tool_name: "tyde_send_agent_message",
                tool_result: json!({ "kind": "Other", "result": { "ok": true } }),
                success: false,
                error: Some("canonical result was invalid"),
            },
            ToolExecutionNormalizationFailure::CanonicalResult,
        );

        let completion = recv_events(&mut rx)
            .into_iter()
            .find(|event| {
                event.get("kind").and_then(Value::as_str) == Some("ToolExecutionCompleted")
            })
            .expect("normalization completion");
        let event: ChatEvent =
            serde_json::from_value(completion).expect("completion remains a typed ChatEvent");
        let ChatEvent::ToolExecutionCompleted(completion) = event else {
            panic!("expected tool completion");
        };
        assert_eq!(
            completion.normalization_failure,
            Some(ToolExecutionNormalizationFailure::CanonicalRequestAndResult)
        );
    }

    #[test]
    fn malformed_canonical_request_emits_failed_tool_completion_without_chat_error() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let emitter = TurnEmitter::new(tx);
        emitter.tool_request(
            "tool-malformed",
            "tyde_send_agent_message",
            json!({ "kind": "Other", "args": { "agent_id": "agent-a" } }),
        );
        emitter.tool_completed(ToolCompletedPayload {
            tool_call_id: "tool-malformed",
            tool_name: "tyde_send_agent_message",
            tool_result: json!({ "kind": "Other", "result": { "ok": true } }),
            success: true,
            error: None,
        });

        let events = recv_events(&mut rx);
        assert!(
            events
                .iter()
                .all(|event| { event.get("kind").and_then(Value::as_str) != Some("Error") })
        );
        let completion = events
            .iter()
            .find(|event| {
                event.get("kind").and_then(Value::as_str) == Some("ToolExecutionCompleted")
            })
            .expect("failed tool completion");
        assert_eq!(
            completion.pointer("/data/success").and_then(Value::as_bool),
            Some(false)
        );
        assert_eq!(
            completion
                .pointer("/data/normalization_failure")
                .and_then(Value::as_str),
            Some("canonical_request")
        );
        assert!(
            completion
                .pointer("/data/error")
                .and_then(Value::as_str)
                .is_some_and(|error| error.contains("expected non-empty agent_id/agentId"))
        );
        assert_protocol_valid(&events);
    }

    #[test]
    fn idle_finalizes_malformed_request_without_provider_completion() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let emitter = TurnEmitter::new(tx);
        let container = emitter.tool_request_in_container(
            "tool-malformed-idle",
            "tyde_send_agent_message",
            json!({ "kind": "Other", "args": {} }),
        );
        emitter.close_tool_container(container.expect("synthetic malformed tool container"));
        emitter.typing_status_changed(false);

        let events = recv_events(&mut rx);
        assert_eq!(
            events
                .iter()
                .filter(|event| {
                    event.get("kind").and_then(Value::as_str) == Some("ToolExecutionCompleted")
                })
                .count(),
            1
        );
        let completion = events
            .iter()
            .find(|event| {
                event.get("kind").and_then(Value::as_str) == Some("ToolExecutionCompleted")
            })
            .expect("idle failed tool completion");
        assert_eq!(
            completion.pointer("/data/success").and_then(Value::as_bool),
            Some(false)
        );
        assert!(
            events
                .iter()
                .all(|event| { event.get("kind").and_then(Value::as_str) != Some("Error") })
        );
        assert_protocol_valid(&events);
    }

    #[test]
    fn correlated_provider_error_completes_malformed_tool_without_chat_error() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let emitter = TurnEmitter::new(tx);
        emitter.tool_request(
            "tool-provider-error",
            "tyde_send_agent_message",
            json!({ "kind": "Other", "args": {} }),
        );

        assert!(emitter.fail_pending_tool("tool-provider-error", "invalid arguments"));
        assert!(!emitter.fail_pending_tool("tool-provider-error", "duplicate error"));

        let events = recv_events(&mut rx);
        assert_eq!(
            events
                .iter()
                .filter(|event| {
                    event.get("kind").and_then(Value::as_str) == Some("ToolExecutionCompleted")
                })
                .count(),
            1
        );
        assert!(
            events
                .iter()
                .all(|event| { event.get("kind").and_then(Value::as_str) != Some("Error") })
        );
        assert_protocol_valid(&events);
    }

    #[test]
    fn unrelated_tool_errors_omit_normalization_failure() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let emitter = TurnEmitter::new(tx);
        emitter.tool_request(
            "tool-unrelated-error",
            "run_command",
            json!({ "kind": "Other", "args": { "command": "false" } }),
        );
        emitter.tool_completed(ToolCompletedPayload {
            tool_call_id: "tool-unrelated-error",
            tool_name: "run_command",
            tool_result: json!({ "kind": "Error", "result": {} }),
            success: false,
            error: Some("exit status 1"),
        });

        let completion = recv_events(&mut rx)
            .into_iter()
            .find(|event| {
                event.get("kind").and_then(Value::as_str) == Some("ToolExecutionCompleted")
            })
            .expect("unrelated completion");
        assert!(
            completion
                .get("data")
                .is_some_and(|data| { data.get("normalization_failure").is_none() })
        );
    }

    fn assert_protocol_valid(events: &[Value]) {
        let mut validator = ProtocolValidator::new();
        let host_stream = StreamPath("/host/local".to_string());
        let agent_stream = StreamPath("/agent/agent-1/instance-1".to_string());
        let agent_id = AgentId("agent-1".to_string());
        let new_agent = NewAgentPayload {
            agent_id: agent_id.clone(),
            name: "Test Agent".to_string(),
            origin: AgentOrigin::User,
            backend_kind: BackendKind::Codex,
            launch_profile_id: None,
            workspace_roots: vec!["/tmp".to_string()],
            custom_agent_id: None,
            team_id: None,
            team_member_id: None,
            project_id: None,
            parent_agent_id: None,
            session_id: None,
            workflow: None,
            created_at_ms: 0,
            instance_stream: agent_stream.clone(),
            activity_summary: Default::default(),
        };
        let welcome = Envelope::from_payload(
            host_stream.clone(),
            FrameKind::Welcome,
            0,
            &WelcomePayload {
                protocol_version: PROTOCOL_VERSION,
                tyde_version: Version {
                    major: 0,
                    minor: 0,
                    patch: 0,
                },
                release_version: None,
            },
        )
        .expect("serialize Welcome");
        validator
            .validate_envelope(&welcome)
            .expect("Welcome validates");
        let bootstrap = Envelope::from_payload(
            host_stream,
            FrameKind::HostBootstrap,
            1,
            &HostBootstrapPayload {
                settings: HostSettings {
                    enabled_backends: vec![BackendKind::Codex],
                    default_backend: Some(BackendKind::Codex),
                    enable_mobile_connections: false,
                    mobile_broker_url: None,
                    tyde_debug_mcp_enabled: false,
                    tyde_agent_control_mcp_enabled: true,
                    complexity_tiers_enabled: false,
                    backend_tier_configs: std::collections::HashMap::new(),
                    background_agent_features: Default::default(),
                    supervisor: Default::default(),
                    code_intel: Default::default(),
                    backend_config: std::collections::HashMap::new(),
                    launch_profiles: Vec::new(),
                },
                mobile_access: MobileAccessStatePayload {
                    broker_status: MobileBrokerStatus::Disabled,
                    pairing: MobilePairingState::Idle,
                    paired_devices: vec![],
                },
                backend_setup: BackendSetupPayload { backends: vec![] },
                session_schemas: vec![],
                backend_config_schemas: vec![],
                backend_config_snapshots: vec![],
                launch_profile_catalog: Default::default(),
                sessions: vec![],
                session_list: Default::default(),
                projects: vec![],
                mcp_servers: vec![],
                skills: vec![],
                steering: vec![],
                custom_agents: vec![],
                team_preset_catalog: TeamPresetCatalog {
                    role_presets: vec![],
                    personality_traits: vec![],
                    personality_presets: vec![],
                    team_templates: vec![],
                },
                team_drafts: vec![],
                teams: vec![],
                team_members: vec![],
                team_member_bindings: vec![],
                agents: vec![new_agent.clone()],
                task_token_usages: Vec::new(),
                workflow_summaries: vec![],
                workflow_diagnostics: vec![],
                workflow_runs: vec![],
                workflow_locations: vec![],
                agents_view_preferences: None,
            },
        )
        .expect("serialize HostBootstrap");
        validator
            .validate_envelope(&bootstrap)
            .expect("HostBootstrap validates");
        let agent_bootstrap = Envelope::from_payload(
            agent_stream.clone(),
            FrameKind::AgentBootstrap,
            0,
            &AgentBootstrapPayload {
                events: vec![AgentBootstrapEvent::AgentStart(AgentStartPayload {
                    agent_id,
                    name: new_agent.name,
                    origin: new_agent.origin,
                    backend_kind: new_agent.backend_kind,
                    launch_profile_id: None,
                    workspace_roots: new_agent.workspace_roots,
                    custom_agent_id: new_agent.custom_agent_id,
                    team_id: new_agent.team_id,
                    team_member_id: new_agent.team_member_id,
                    project_id: new_agent.project_id,
                    parent_agent_id: new_agent.parent_agent_id,
                    session_id: None,
                    workflow: None,
                    created_at_ms: new_agent.created_at_ms,
                })],
                latest_output: Default::default(),
            },
        )
        .expect("serialize AgentBootstrap");
        validator
            .validate_envelope(&agent_bootstrap)
            .expect("AgentBootstrap validates");

        for (index, event) in events.iter().enumerate() {
            let chat_event: ChatEvent =
                serde_json::from_value(event.clone()).expect("emitter produced ChatEvent JSON");
            let envelope = Envelope::from_payload(
                agent_stream.clone(),
                FrameKind::ChatEvent,
                index as u64 + 1,
                &chat_event,
            )
            .expect("serialize ChatEvent");
            validator
                .validate_envelope(&envelope)
                .unwrap_or_else(|err| panic!("event {index} violates protocol: {err}"));
        }
    }

    fn run_command_request() -> Value {
        json!({
            "kind": "RunCommand",
            "command": "echo ok",
            "working_directory": "/tmp",
        })
    }

    fn read_files_request() -> Value {
        json!({
            "kind": "ReadFiles",
            "file_paths": ["/tmp/file.txt"],
        })
    }

    #[test]
    fn cancel_mid_stream_emits_stream_end_then_cancel_then_typing_false() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let emitter = TurnEmitter::new(tx);
        emitter.stream_start("msg-1", AgentName("claude"), None);
        emitter.operation_cancelled("bye");
        drop(emitter);
        assert_eq!(
            recv_kinds(&mut rx),
            vec![
                "StreamStart",
                "StreamEnd",
                "OperationCancelled",
                "TypingStatusChanged",
            ]
        );
    }

    #[test]
    fn cancel_with_pending_tools_emits_tool_completed_before_cancel() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let emitter = TurnEmitter::new(tx);
        emitter.stream_start("msg-1", AgentName("claude"), None);
        emitter.stream_end(StreamEndPayload::default());
        emitter.tool_request("tool-a", "Bash", run_command_request());
        emitter.tool_request("tool-b", "Read", read_files_request());
        emitter.operation_cancelled("bye");
        drop(emitter);
        let kinds = recv_kinds(&mut rx);
        assert_eq!(
            kinds,
            vec![
                "StreamStart",
                "StreamEnd",
                "ToolRequest",
                "ToolRequest",
                "ToolExecutionCompleted",
                "ToolExecutionCompleted",
                "OperationCancelled",
                "TypingStatusChanged",
            ]
        );
    }

    #[test]
    fn already_completed_tools_are_not_re_completed_on_cancel() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let emitter = TurnEmitter::new(tx);
        emitter.tool_request("tool-a", "Bash", run_command_request());
        emitter.tool_completed(ToolCompletedPayload {
            tool_call_id: "tool-a",
            tool_name: "Bash",
            tool_result: json!({
                "kind": "Other",
                "result": {},
            }),
            success: true,
            error: None,
        });
        emitter.operation_cancelled("bye");
        drop(emitter);
        let events = recv_events(&mut rx);
        assert_protocol_valid(&events);
        let kinds = event_kinds(&events);
        assert_eq!(
            kinds,
            vec![
                "StreamStart",
                "ToolRequest",
                "ToolExecutionCompleted",
                "StreamEnd",
                "OperationCancelled",
                "TypingStatusChanged",
            ]
        );
    }

    #[test]
    fn stream_end_without_open_stream_reports_identity_violation() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let emitter = TurnEmitter::new(tx);
        emitter.stream_end(StreamEndPayload::default());
        drop(emitter);
        let events = recv_events(&mut rx);
        assert_eq!(event_kinds(&events), vec!["Error"]);
        assert_eq!(
            events[0].get("data").and_then(Value::as_str),
            Some("Stream identity violation: missing message id")
        );
    }

    #[test]
    fn stream_end_carries_active_stream_message_id() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let emitter = TurnEmitter::new_for_agent(tx, AgentName("codex"));
        emitter.stream_start("message-1", AgentName("codex"), Some("gpt-5-codex"));
        emitter.stream_delta("message-1", "hello");
        emitter.stream_end(StreamEndPayload {
            content: "hello".to_string(),
            agent: Some(AgentName("codex")),
            model: Some("gpt-5-codex".to_string()),
            request_usage: None,
            turn_usage: None,
            cumulative_usage: None,
            token_usage_unavailable_reason: None,
            reasoning: None,
            tool_calls: Vec::new(),
            context_breakdown: None,
            images: Vec::new(),
        });
        drop(emitter);
        let events = recv_events(&mut rx);
        assert_protocol_valid(&events);
        assert_eq!(
            events[2]
                .pointer("/data/message/message_id")
                .and_then(Value::as_str),
            Some("message-1")
        );
    }

    #[test]
    fn message_metadata_updated_emits_patch_event() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let emitter = TurnEmitter::new(tx);
        emitter.message_metadata_updated(MessageMetadataUpdatePayload {
            message_id: "message-1".to_string(),
            model_info: Some(json!({ "model": "gpt-5-codex" })),
            request_usage: Some(json!({
                "input_tokens": 1,
                "output_tokens": 2,
                "total_tokens": 3,
                "cached_prompt_tokens": 0,
                "cache_creation_input_tokens": 0,
                "reasoning_tokens": 0
            })),
            turn_usage: Some(json!({
                "input_tokens": 1,
                "output_tokens": 2,
                "total_tokens": 3,
                "cached_prompt_tokens": 0,
                "cache_creation_input_tokens": 0,
                "reasoning_tokens": 0
            })),
            cumulative_usage: None,
            context_breakdown: None,
        });
        drop(emitter);
        let events = recv_events(&mut rx);
        assert_eq!(event_kinds(&events), vec!["MessageMetadataUpdated"]);
        assert_eq!(
            events[0]
                .pointer("/data/message_id")
                .and_then(Value::as_str),
            Some("message-1")
        );
        assert_eq!(
            events[0]
                .pointer("/data/token_usage/request/usage/total_tokens")
                .and_then(Value::as_u64),
            Some(3)
        );
        assert_eq!(
            events[0]
                .pointer("/data/token_usage/turn/usage/total_tokens")
                .and_then(Value::as_u64),
            Some(3)
        );
    }

    #[test]
    fn stream_end_emits_explicit_unavailable_token_usage_when_requested() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let emitter = TurnEmitter::new(tx);
        emitter.stream_start("message-1", AgentName("assistant"), None);
        emitter.stream_end(StreamEndPayload {
            content: "done".to_string(),
            token_usage_unavailable_reason: Some(TokenUsageUnavailableReason::BackendDidNotReport),
            ..StreamEndPayload::default()
        });
        drop(emitter);

        let events = recv_events(&mut rx);
        assert_protocol_valid(&events);
        let stream_end = events
            .iter()
            .find(|event| event.get("kind").and_then(Value::as_str) == Some("StreamEnd"))
            .expect("StreamEnd event");
        assert_eq!(
            stream_end.pointer("/data/message/token_usage/request/kind"),
            Some(&Value::String("unavailable".to_string()))
        );
        assert_eq!(
            stream_end
                .pointer("/data/message/token_usage/turn/reason")
                .and_then(Value::as_str),
            Some("backend_did_not_report")
        );
        assert_eq!(
            stream_end
                .pointer("/data/message/token_usage/cumulative/reason")
                .and_then(Value::as_str),
            Some("backend_did_not_report")
        );
    }

    #[test]
    fn mixed_token_usage_preserves_known_turn_and_provider_scope_reason() {
        let token_usage = build_message_token_usage_value(
            None,
            Some(json!({ "total_tokens": 12 })),
            None,
            Some(TokenUsageUnavailableReason::ProviderScopeAmbiguous),
        );

        assert_eq!(
            token_usage.pointer("/turn/kind"),
            Some(&Value::String("known".to_owned()))
        );
        assert_eq!(
            token_usage.pointer("/turn/usage/total_tokens"),
            Some(&Value::from(12))
        );
        assert_eq!(
            token_usage.pointer("/request/reason"),
            Some(&Value::String("provider_scope_ambiguous".to_owned()))
        );
        assert_eq!(
            token_usage.pointer("/cumulative/reason"),
            Some(&Value::String("provider_scope_ambiguous".to_owned()))
        );
    }

    #[test]
    fn mixed_token_usage_defaults_absent_scopes_to_backend_not_reported() {
        let token_usage =
            build_message_token_usage_value(None, Some(json!({ "total_tokens": 12 })), None, None);

        assert_eq!(
            token_usage.pointer("/turn/kind"),
            Some(&Value::String("known".to_owned()))
        );
        assert_eq!(
            token_usage.pointer("/request/reason"),
            Some(&Value::String("backend_did_not_report".to_owned()))
        );
        assert_eq!(
            token_usage.pointer("/cumulative/reason"),
            Some(&Value::String("backend_did_not_report".to_owned()))
        );
    }

    #[test]
    fn cancel_after_stream_end_does_not_synthesize_second_stream_end() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let emitter = TurnEmitter::new(tx);
        emitter.stream_start("msg-1", AgentName("claude"), None);
        emitter.stream_end(StreamEndPayload::default());
        emitter.operation_cancelled("bye");
        drop(emitter);
        let kinds = recv_kinds(&mut rx);
        assert_eq!(
            kinds,
            vec![
                "StreamStart",
                "StreamEnd",
                "OperationCancelled",
                "TypingStatusChanged",
            ]
        );
    }

    #[test]
    fn second_turn_after_cancel_starts_clean() {
        // The emitter lives for the lifetime of the backend, so after a
        // cancel the next turn's tool requests must not inherit the
        // previous turn's "pending" set (that would double-emit
        // cancellations on the next cancel).
        let (tx, mut rx) = mpsc::unbounded_channel();
        let emitter = TurnEmitter::new(tx);

        // Turn 1: open a tool request, cancel.
        emitter.stream_start("msg-1", AgentName("claude"), None);
        emitter.tool_request("tool-a", "Bash", run_command_request());
        emitter.operation_cancelled("stop");
        // Drain.
        let _ = recv_kinds(&mut rx);

        // Turn 2: a new stream, a new tool, then a second cancel.
        emitter.stream_start("msg-2", AgentName("claude"), None);
        emitter.tool_request("tool-b", "Read", read_files_request());
        emitter.operation_cancelled("stop again");

        let kinds = recv_kinds(&mut rx);
        assert_eq!(
            kinds,
            vec![
                "StreamStart",
                "ToolRequest",
                "StreamEnd",
                "ToolExecutionCompleted", // only tool-b, not tool-a
                "OperationCancelled",
                "TypingStatusChanged",
            ]
        );
    }

    #[test]
    fn stream_delta_is_suppressed_when_text_empty() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let emitter = TurnEmitter::new(tx);
        emitter.stream_start("msg-1", AgentName("assistant"), None);
        emitter.stream_delta("msg-1", "");
        emitter.stream_delta("msg-1", "hi");
        drop(emitter);
        let events = recv_events(&mut rx);
        assert_protocol_valid(&events);
        assert_eq!(event_kinds(&events), vec!["StreamStart", "StreamDelta"]);
    }

    #[test]
    fn reasoning_delta_without_open_stream_reports_identity_violation() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let emitter = TurnEmitter::new_for_agent(tx, AgentName("codex"));
        emitter.stream_reasoning_delta("reason-1", "thinking");
        drop(emitter);
        let events = recv_events(&mut rx);
        assert_eq!(event_kinds(&events), vec!["Error"]);
        assert_eq!(
            events[0].get("data").and_then(Value::as_str),
            Some("Stream identity violation: foreign active message id")
        );
    }

    #[test]
    fn foreign_delta_discards_the_open_stream_without_a_fabricated_completion() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let emitter = TurnEmitter::new_for_agent(tx, AgentName("codex"));
        emitter.stream_start("message-1", AgentName("codex"), None);
        emitter.stream_delta("message-2", "foreign");
        drop(emitter);

        let events = recv_events(&mut rx);
        assert_eq!(
            event_kinds(&events),
            vec![
                "StreamStart",
                "Error",
                "OperationCancelled",
                "TypingStatusChanged",
            ]
        );
        assert!(event_kinds(&events).iter().all(|kind| kind != "StreamEnd"));
        assert_eq!(
            events[1].get("data").and_then(Value::as_str),
            Some("Stream identity violation: foreign active message id")
        );
    }

    #[test]
    fn terminal_stream_message_id_cannot_be_reused() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let emitter = TurnEmitter::new(tx);
        emitter.stream_start("message-1", AgentName("assistant"), None);
        emitter.stream_end(StreamEndPayload::default());
        emitter.stream_start("message-1", AgentName("assistant"), None);
        drop(emitter);

        let events = recv_events(&mut rx);
        assert_eq!(
            event_kinds(&events),
            vec!["StreamStart", "StreamEnd", "Error"]
        );
        assert_eq!(
            events[2].get("data").and_then(Value::as_str),
            Some("Stream identity violation: duplicate terminal message id")
        );
    }

    #[test]
    fn typed_stream_end_rejects_a_foreign_id_and_reports_once_per_turn() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let emitter = TurnEmitter::new(tx);
        emitter.stream_start_with_id(
            ChatMessageId("message-1".to_owned()),
            AgentName("assistant"),
            None,
        );
        emitter.stream_end_with_id(
            ChatMessageId("message-2".to_owned()),
            StreamEndPayload::default(),
        );
        emitter.stream_end_with_id(
            ChatMessageId("message-1".to_owned()),
            StreamEndPayload::default(),
        );
        drop(emitter);

        let events = recv_events(&mut rx);
        assert_eq!(
            event_kinds(&events),
            vec![
                "StreamStart",
                "Error",
                "OperationCancelled",
                "TypingStatusChanged",
            ]
        );
        assert!(event_kinds(&events).iter().all(|kind| kind != "StreamEnd"));
        assert_eq!(
            event_kinds(&events)
                .iter()
                .filter(|kind| kind.as_str() == "Error")
                .count(),
            1
        );
    }

    #[test]
    fn typed_discard_cancels_without_stream_end_and_allows_the_next_stream() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let emitter = TurnEmitter::new(tx);
        emitter.stream_start_with_id(
            ChatMessageId("discarded-message".to_owned()),
            AgentName("assistant"),
            None,
        );
        emitter.discard_open_stream_with_identity_violation(
            StreamIdentityViolation::ForeignActiveMessageId,
        );
        emitter.stream_start_with_id(
            ChatMessageId("next-message".to_owned()),
            AgentName("assistant"),
            None,
        );
        drop(emitter);

        let events = recv_events(&mut rx);
        assert_eq!(
            event_kinds(&events),
            vec![
                "StreamStart",
                "Error",
                "OperationCancelled",
                "TypingStatusChanged",
                "StreamStart",
            ]
        );
        assert!(event_kinds(&events).iter().all(|kind| kind != "StreamEnd"));
    }

    #[test]
    fn generated_identity_contract_is_carried_through_typed_stream_end() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let emitter = TurnEmitter::new(tx);
        let identity = ServerGeneratedChatMessageIdentity {
            origin: ServerGeneratedChatMessageIdOrigin::IdlessReasoning,
            stream_epoch: 4,
            item_ordinal: 2,
        };
        let message_id = identity.message_id();

        emitter.stream_start_with_generated_identity(&identity, AgentName("assistant"), None);
        emitter.stream_reasoning_delta_with_id(message_id.clone(), "thinking");
        emitter.stream_end_with_id(message_id, StreamEndPayload::default());
        drop(emitter);

        let events = recv_events(&mut rx);
        assert_eq!(
            events[0]
                .pointer("/data/message_id")
                .and_then(Value::as_str),
            Some("server-generated:idless_reasoning:4:2")
        );
        assert_eq!(
            events[2]
                .pointer("/data/message/message_id")
                .and_then(Value::as_str),
            Some("server-generated:idless_reasoning:4:2")
        );
    }

    #[test]
    fn second_stream_start_is_rejected_without_rebinding_the_active_stream() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let emitter = TurnEmitter::new_for_agent(tx, AgentName("codex"));
        emitter.stream_start("msg-1", AgentName("codex"), Some("model-a"));
        emitter.stream_start("msg-2", AgentName("codex"), Some("model-a"));
        drop(emitter);
        let events = recv_events(&mut rx);
        assert_eq!(
            event_kinds(&events),
            vec![
                "StreamStart",
                "Error",
                "OperationCancelled",
                "TypingStatusChanged",
            ]
        );
    }

    #[test]
    fn tool_request_without_assistant_turn_synthesizes_stream_start() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let emitter = TurnEmitter::new_for_agent(tx, AgentName("codex"));
        emitter.tool_request("tool-a", "run_command", run_command_request());
        drop(emitter);
        let events = recv_events(&mut rx);
        assert_protocol_valid(&events);
        assert_eq!(event_kinds(&events), vec!["StreamStart", "ToolRequest"]);
    }

    #[test]
    fn shared_emitter_normalizes_tyde_spawn_and_emits_typed_progress() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let emitter = TurnEmitter::new_for_agent(tx, AgentName("provider"));
        emitter.tool_request(
            "spawn-1",
            "mcp__tyde-agent-control__tyde_spawn_agent",
            json!({
                "kind": "Other",
                "args": {"arguments": {"prompt": "Inspect the parser", "name": "Parser"}}
            }),
        );
        emitter.tool_completed(ToolCompletedPayload {
            tool_call_id: "spawn-1",
            tool_name: "mcp__tyde-agent-control__tyde_spawn_agent",
            tool_result: json!({
                "kind": "Other",
                "result": {"agent_id": "child-1", "name": "Parser"}
            }),
            success: true,
            error: None,
        });
        drop(emitter);

        let events = recv_events(&mut rx);
        assert_eq!(
            events[1]
                .pointer("/data/tool_type/kind")
                .and_then(Value::as_str),
            Some("AgentSpawn")
        );
        assert_eq!(
            events[1]
                .pointer("/data/tool_type/prompt")
                .and_then(Value::as_str),
            Some("Inspect the parser")
        );
        assert_eq!(
            events[2].get("kind").and_then(Value::as_str),
            Some("ToolProgress")
        );
        assert_eq!(
            events[2]
                .pointer("/data/update/progress_kind")
                .and_then(Value::as_str),
            Some("spawn")
        );
        assert_eq!(
            events[2]
                .pointer("/data/update/agents/0/agent_id")
                .and_then(Value::as_str),
            Some("child-1")
        );
    }

    #[test]
    fn shared_emitter_rejects_progress_for_a_different_tool_identity() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let emitter = TurnEmitter::new_for_agent(tx, AgentName("provider"));
        emitter.tool_request("shared-id", "run_command", run_command_request());
        emitter.tool_progress(&protocol::ToolProgressData {
            tool_call_id: "shared-id".to_owned(),
            tool_name: "tyde_spawn_agent".to_owned(),
            update: protocol::ToolProgressUpdate::AgentControl(protocol::AgentControlProgress {
                progress_kind: protocol::AgentControlProgressKind::Spawn,
                agents: vec![protocol::AgentControlAgentRef {
                    agent_id: protocol::AgentId("child-1".to_owned()),
                    name: Some("Child".to_owned()),
                }],
            }),
        });
        drop(emitter);

        let events = recv_events(&mut rx);
        assert_eq!(
            event_kinds(&events),
            vec!["StreamStart", "ToolRequest", "Error"]
        );
        assert!(
            events
                .iter()
                .all(|event| { event.get("kind").and_then(Value::as_str) != Some("ToolProgress") })
        );
    }

    #[test]
    fn exact_duplicate_completed_tool_request_is_idempotent() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let emitter = TurnEmitter::new_for_agent(tx, AgentName("codex"));
        let request = run_command_request();
        let container = emitter
            .tool_request_in_container("tool-a", "run_command", request.clone())
            .expect("tool-first request must open a container");
        emitter.tool_completed(ToolCompletedPayload {
            tool_call_id: "tool-a",
            tool_name: "run_command",
            tool_result: json!({
                "kind": "RunCommand",
                "exit_code": 0,
                "stdout": "",
                "stderr": "",
            }),
            success: true,
            error: None,
        });
        emitter.close_tool_container(container);
        emitter.tool_completed(ToolCompletedPayload {
            tool_call_id: "tool-a",
            tool_name: "run_command",
            tool_result: json!({
                "kind": "RunCommand",
                "exit_code": 0,
                "stdout": "",
                "stderr": "",
            }),
            success: true,
            error: None,
        });
        assert!(
            emitter
                .tool_request_in_container("tool-a", "run_command", request)
                .is_none()
        );
        drop(emitter);

        let events = recv_events(&mut rx);
        assert_protocol_valid(&events);
        assert_eq!(
            event_kinds(&events),
            vec![
                "StreamStart",
                "ToolRequest",
                "ToolExecutionCompleted",
                "StreamEnd",
            ]
        );
        assert!(
            events
                .iter()
                .all(|event| { event.get("kind").and_then(Value::as_str) != Some("Error") })
        );
    }

    #[test]
    fn explicit_tool_container_closes_before_real_stream_without_losing_tool_card() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let emitter = TurnEmitter::new_for_agent(tx, AgentName("codex"));
        let container = emitter
            .tool_request_in_container("tool-a", "run_command", run_command_request())
            .expect("tool-first request must open an explicit container");
        emitter.tool_completed(ToolCompletedPayload {
            tool_call_id: "tool-a",
            tool_name: "run_command",
            tool_result: json!({
                "kind": "RunCommand",
                "exit_code": 0,
                "stdout": "",
                "stderr": "",
            }),
            success: true,
            error: None,
        });
        emitter.close_tool_container(container);
        let real_message_id = ChatMessageId("msg-real".to_owned());
        emitter.stream_start_with_id(real_message_id.clone(), AgentName("codex"), Some("model-a"));
        emitter.stream_delta_with_id(real_message_id.clone(), "done");
        emitter.stream_end_with_id(
            real_message_id,
            StreamEndPayload {
                content: "done".to_owned(),
                ..StreamEndPayload::default()
            },
        );
        drop(emitter);
        let events = recv_events(&mut rx);
        assert_protocol_valid(&events);
        assert_eq!(
            event_kinds(&events),
            vec![
                "StreamStart",
                "ToolRequest",
                "ToolExecutionCompleted",
                "StreamEnd",
                "StreamStart",
                "StreamDelta",
                "StreamEnd",
            ]
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event.get("kind").and_then(Value::as_str) == Some("ToolRequest"))
                .count(),
            1
        );
        let container_end = events
            .iter()
            .find(|event| {
                event.get("kind").and_then(Value::as_str) == Some("StreamEnd")
                    && event
                        .pointer("/data/message/message_id")
                        .and_then(Value::as_str)
                        == Some("tool-a")
            })
            .expect("tool container StreamEnd");
        assert_eq!(
            container_end
                .pointer("/data/message/tool_calls/0/id")
                .and_then(Value::as_str),
            Some("tool-a")
        );
    }

    #[test]
    fn later_tool_in_same_turn_opens_its_own_message_container() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let emitter = TurnEmitter::new_for_agent(tx, AgentName("codex"));
        let first_message_id = ChatMessageId("msg-first".to_owned());
        emitter.stream_start_with_id(first_message_id.clone(), AgentName("codex"), Some("model"));
        emitter.stream_end_with_id(
            first_message_id,
            StreamEndPayload {
                content: "I will run the tool.".to_owned(),
                ..StreamEndPayload::default()
            },
        );

        let container = match emitter.tool_request_in_container(
            "tool-later",
            "run_command",
            run_command_request(),
        ) {
            Some(container) => container,
            None => panic!(
                "tool container missing; stream_open={} events={}",
                emitter.has_open_stream(),
                serde_json::to_string(&recv_events(&mut rx)).expect("serialize diagnostics")
            ),
        };
        emitter.tool_completed(ToolCompletedPayload {
            tool_call_id: "tool-later",
            tool_name: "run_command",
            tool_result: json!({
                "kind": "RunCommand",
                "exit_code": 0,
                "stdout": "done",
                "stderr": "",
            }),
            success: true,
            error: None,
        });
        emitter.close_tool_container(container);
        drop(emitter);

        let events = recv_events(&mut rx);
        assert_protocol_valid(&events);
        assert_eq!(
            event_kinds(&events),
            vec![
                "StreamStart",
                "StreamEnd",
                "StreamStart",
                "ToolRequest",
                "ToolExecutionCompleted",
                "StreamEnd",
            ]
        );
    }

    #[test]
    fn pending_tool_completes_during_later_message_without_cancellation() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let emitter = TurnEmitter::new_for_agent(tx, AgentName("codex"));

        let first_message_id = ChatMessageId("msg-first".to_owned());
        emitter.stream_start_with_id(
            first_message_id.clone(),
            AgentName("codex"),
            Some("gpt-5.6-luna"),
        );
        emitter.stream_end_with_id(
            first_message_id,
            StreamEndPayload {
                content: "Starting the command.".to_owned(),
                ..StreamEndPayload::default()
            },
        );
        emitter.tool_request("tool-background", "run_command", run_command_request());

        let second_message_id = ChatMessageId("msg-second".to_owned());
        emitter.stream_start_with_id(
            second_message_id.clone(),
            AgentName("codex"),
            Some("gpt-5.6-luna"),
        );
        emitter
            .stream_reasoning_delta_with_id(second_message_id.clone(), "Waiting for the process.");
        emitter.tool_completed(ToolCompletedPayload {
            tool_call_id: "tool-background",
            tool_name: "run_command",
            tool_result: json!({
                "kind": "RunCommand",
                "exit_code": 0,
                "stdout": "done",
                "stderr": "",
            }),
            success: true,
            error: None,
        });
        emitter.stream_end_with_id(
            second_message_id,
            StreamEndPayload {
                content: "Done.".to_owned(),
                ..StreamEndPayload::default()
            },
        );

        drop(emitter);
        let events = recv_events(&mut rx);
        assert_protocol_valid(&events);
        assert_eq!(
            event_kinds(&events),
            vec![
                "StreamStart",
                "StreamEnd",
                "ToolRequest",
                "StreamStart",
                "StreamReasoningDelta",
                "ToolExecutionCompleted",
                "StreamEnd",
            ]
        );
        let completions = events
            .iter()
            .filter(|event| {
                event.get("kind").and_then(Value::as_str) == Some("ToolExecutionCompleted")
            })
            .collect::<Vec<_>>();
        assert_eq!(completions.len(), 1);
        assert_eq!(
            completions[0]
                .pointer("/data/tool_call_id")
                .and_then(Value::as_str),
            Some("tool-background")
        );
        assert_eq!(
            completions[0]
                .pointer("/data/success")
                .and_then(Value::as_bool),
            Some(true)
        );
    }

    #[test]
    fn detached_tool_survives_unrelated_turn_cancellation_and_completes_once() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let emitter = TurnEmitter::new_for_agent(tx, AgentName("claude"));
        emitter.tool_request("tool-background", "Bash", run_command_request());
        assert!(emitter.detach_tool("tool-background"));

        emitter.operation_cancelled("later turn cancelled");
        emitter.tool_completed(ToolCompletedPayload {
            tool_call_id: "tool-background",
            tool_name: "Bash",
            tool_result: json!({
                "kind": "RunCommand",
                "exit_code": 0,
                "stdout": "done",
                "stderr": "",
            }),
            success: true,
            error: None,
        });
        drop(emitter);

        let events = recv_events(&mut rx);
        assert_protocol_valid(&events);
        let completions = events
            .iter()
            .filter(|event| event.get("kind").and_then(Value::as_str) == Some("ToolExecutionCompleted"))
            .collect::<Vec<_>>();
        assert_eq!(completions.len(), 1);
        assert_eq!(
            completions[0]
                .pointer("/data/tool_call_id")
                .and_then(Value::as_str),
            Some("tool-background")
        );
        assert_eq!(
            completions[0].pointer("/data/success").and_then(Value::as_bool),
            Some(true)
        );
    }

    #[test]
    fn unknown_tool_completion_synthesizes_matching_request() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let emitter = TurnEmitter::new_for_agent(tx, AgentName("codex"));
        emitter.tool_completed(ToolCompletedPayload {
            tool_call_id: "tool-a",
            tool_name: "run_command",
            tool_result: json!({
                "kind": "RunCommand",
                "exit_code": 0,
                "stdout": "",
                "stderr": "",
            }),
            success: true,
            error: None,
        });
        drop(emitter);
        let events = recv_events(&mut rx);
        assert_protocol_valid(&events);
        assert_eq!(
            event_kinds(&events),
            vec!["StreamStart", "ToolRequest", "ToolExecutionCompleted"]
        );
    }

    fn sample_progress(tool_call_id: &str) -> protocol::ToolProgressData {
        protocol::ToolProgressData {
            tool_call_id: tool_call_id.to_string(),
            tool_name: "Workflow".to_string(),
            update: protocol::ToolProgressUpdate::Workflow(protocol::WorkflowRunState {
                workflow_name: "probe".to_string(),
                description: None,
                script: None,
                status: protocol::WorkflowRunStatus::Running,
                summary: None,
                total_tokens: 1,
                tool_uses: 0,
                duration_ms: 10,
                agents: vec![],
            }),
        }
    }

    #[test]
    fn tool_progress_emits_at_any_lifecycle_point() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let emitter = TurnEmitter::new(tx);
        // Before any request is known.
        emitter.tool_progress(&sample_progress("tool-a"));
        emitter.tool_request("tool-a", "Workflow", json!({"kind": "Other", "args": {}}));
        // Between request and completion.
        emitter.tool_progress(&sample_progress("tool-a"));
        emitter.tool_completed(ToolCompletedPayload {
            tool_call_id: "tool-a",
            tool_name: "Workflow",
            tool_result: json!({"kind": "Other", "result": {}}),
            success: true,
            error: None,
        });
        // After completion — the background task is still running.
        emitter.tool_progress(&sample_progress("tool-a"));
        // After a turn reset, when per-turn tool maps are cleared.
        emitter.operation_cancelled("bye");
        emitter.tool_progress(&sample_progress("tool-a"));
        drop(emitter);
        let events = recv_events(&mut rx);
        assert_protocol_valid(&events);
        assert_eq!(
            event_kinds(&events)
                .iter()
                .filter(|kind| *kind == "ToolProgress")
                .count(),
            4
        );
    }

    #[test]
    fn tool_progress_with_empty_id_is_dropped() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let emitter = TurnEmitter::new(tx);
        emitter.tool_progress(&sample_progress(""));
        drop(emitter);
        assert_eq!(recv_kinds(&mut rx), Vec::<String>::new());
    }
}
