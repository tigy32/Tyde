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

use indexmap::IndexSet;

use indexmap::IndexMap;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use protocol::types::StreamIdentityViolation;
use protocol::{
    ChatMessageId, ModelRequestTokenUsage, ServerGeneratedChatMessageIdentity, TaskList,
    TokenUsageUnavailableReason, ToolExecutionNormalizationFailure,
};

use super::agent_control_progress::{
    await_progress_data_for_tool, spawn_progress_data_for_tool_result,
    terminal_await_progress_data_for_tool, tyde_tool_request_type, tyde_tool_result,
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
    typing_active: bool,
    current_stream_message_id: Option<ChatMessageId>,
    synthetic_tool_container_id: Option<ChatMessageId>,
    synthetic_tool_container_prior_assistant_turn_open: Option<bool>,
    synthetic_tool_call_ids: Vec<String>,
    declared_tool_owners: HashMap<String, ChatMessageId>,
    terminal_stream_message_ids: HashSet<ChatMessageId>,
    identity_violation_reported: bool,
    emitted_tool_requests: IndexMap<String, EmittedToolRequest>,
    detached_tool_requests: IndexMap<String, EmittedToolRequest>,
    completed_tool_requests: HashSet<String>,
    /// Completed ids carried across `reset_turn_state`. Terminal message
    /// ids are remembered forever and a synthetic container's message id
    /// IS its tool call id, so completion memory must survive on the same
    /// clock: a late duplicate completion after a reset would otherwise
    /// re-synthesize a container over a terminal id and surface a lone
    /// "duplicate terminal message id" Error card. Bounded FIFO.
    retired_tool_call_ids: IndexSet<String>,
    normalization_failures: HashMap<String, PendingToolNormalizationFailure>,
}

/// Bounds `retired_tool_call_ids` so an arbitrarily long session cannot
/// grow the ledger unboundedly; old entries evict first-in-first-out.
const RETIRED_TOOL_CALL_LEDGER_CAP: usize = 1024;

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
                typing_active: false,
                current_stream_message_id: None,
                synthetic_tool_container_id: None,
                synthetic_tool_container_prior_assistant_turn_open: None,
                synthetic_tool_call_ids: Vec::new(),
                declared_tool_owners: HashMap::new(),
                terminal_stream_message_ids: HashSet::new(),
                identity_violation_reported: false,
                emitted_tool_requests: IndexMap::new(),
                detached_tool_requests: IndexMap::new(),
                completed_tool_requests: HashSet::new(),
                retired_tool_call_ids: IndexSet::new(),
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
            .tool_request(tool_call_id, tool_name, tool_type, None, false, true);
    }

    pub fn tool_request_for_declared_response(
        &self,
        tool_call_id: &str,
        tool_name: &str,
        tool_type: Value,
    ) -> bool {
        self.lock()
            .tool_request_for_declared_response(tool_call_id, tool_name, tool_type)
    }

    pub fn tool_request_in_container(
        &self,
        tool_call_id: &str,
        tool_name: &str,
        tool_type: Value,
    ) -> Option<ChatMessageId> {
        self.lock()
            .tool_request(tool_call_id, tool_name, tool_type, None, true, true)
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
            true,
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

    pub(crate) fn has_known_tool_request(&self, tool_call_id: &str) -> bool {
        let state = self.lock();
        state.is_tool_pending(tool_call_id)
            || state.completed_tool_requests.contains(tool_call_id)
            || state.retired_tool_call_ids.contains(tool_call_id)
    }

    pub(crate) fn has_pending_detached_tools(&self) -> bool {
        !self.lock().detached_tool_requests.is_empty()
    }

    pub(crate) fn is_tool_detached(&self, tool_call_id: &str) -> bool {
        self.lock()
            .detached_tool_requests
            .contains_key(tool_call_id)
    }

    pub(crate) fn tool_request_name(&self, tool_call_id: &str) -> Option<String> {
        let state = self.lock();
        state
            .emitted_tool_requests
            .get(tool_call_id)
            .or_else(|| state.detached_tool_requests.get(tool_call_id))
            .map(|request| request.name.clone())
    }

    pub(crate) fn tool_request_command(&self, tool_call_id: &str) -> Option<String> {
        let state = self.lock();
        let request = state
            .emitted_tool_requests
            .get(tool_call_id)
            .or_else(|| state.detached_tool_requests.get(tool_call_id))?;
        let command = match request.arguments.get("kind").and_then(Value::as_str) {
            Some("RunCommand") => request.arguments.get("command"),
            Some("Other") => request
                .arguments
                .pointer("/args/arguments/command")
                .or_else(|| request.arguments.pointer("/args/command")),
            _ => None,
        };
        command
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|command| !command.is_empty())
            .map(str::to_owned)
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

    pub fn cancel_pending_tool(&self, tool_call_id: &str, message: &str) -> bool {
        let mut state = self.lock();
        let Some(tool_name) = state.pending_tool_name(tool_call_id).cloned() else {
            return false;
        };
        state.send_tool_completed(
            tool_call_id,
            &tool_name,
            cancelled_tool_result(message),
            false,
            Some("Cancelled"),
        );
        true
    }

    pub fn cancel_pending_foreground_tools(&self, message: &str) {
        let mut state = self.lock();
        let pending = state
            .emitted_tool_requests
            .iter()
            .filter(|(tool_call_id, _)| !state.completed_tool_requests.contains(*tool_call_id))
            .map(|(tool_call_id, request)| (tool_call_id.clone(), request.name.clone()))
            .collect::<Vec<_>>();
        for (tool_call_id, tool_name) in pending {
            state.send_tool_completed(
                &tool_call_id,
                &tool_name,
                cancelled_tool_result(message),
                false,
                Some("Cancelled"),
            );
        }
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

    pub fn interrupt_acknowledged(&self, message: &str) {
        // Background tools still own their pending completion, so this cannot
        // use the normal cancellation tail that closes every pending tool.
        let mut state = self.lock();
        state.send(json!({
            "kind": "OperationCancelled",
            "data": { "message": message },
        }));
        state.send(json!({
            "kind": "TypingStatusChanged",
            "data": false,
        }));
        state.typing_active = false;
    }

    pub(crate) fn close(&self, message: &str) {
        let mut state = self.lock();
        state.close(message);
        let (replacement, receiver) = mpsc::unbounded_channel();
        drop(receiver);
        state.tx = replacement;
    }

    // ---------- Messages (user / assistant / system / error / warning) ----------

    pub fn user_message(&self, content: &str, images: Option<Vec<Value>>) {
        let mut state = self.lock();
        state.set_assistant_turn_open(false);
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
        let state = self.lock();
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
        let state = self.lock();
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
        state.set_assistant_turn_open(false);
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

    pub(crate) fn compaction_event(&self, event: &super::compaction::BackendCompactionEvent) {
        let data = serde_json::to_value(event).expect("backend compaction event must serialize");
        self.lock().send(json!({
            "kind": "BackendCompaction",
            "data": data,
        }));
    }

    // ---------- Misc chat events ----------

    pub fn typing_status_changed(&self, typing: bool) {
        let mut state = self.lock();
        if typing && state.typing_active {
            tracing::warn!("suppressed duplicate TypingStatusChanged(true)");
            return;
        }
        if !typing {
            state.complete_pending_normalization_failures();
        }
        state.send(json!({
            "kind": "TypingStatusChanged",
            "data": typing,
        }));
        state.typing_active = typing;
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
        state.typing_active = false;
        state.detached_tool_requests.clear();
        state.declared_tool_owners.clear();
        state.terminal_stream_message_ids.clear();
        // Terminal ids and retired ids live on the same clock: a cleared
        // conversation forgets both together.
        state.retired_tool_call_ids.clear();
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
        self.set_assistant_turn_open(true);
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
        for tool_call in &payload.tool_calls {
            if let Some(tool_call_id) = tool_call
                .get("id")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|tool_call_id| !tool_call_id.is_empty())
            {
                self.declared_tool_owners
                    .insert(tool_call_id.to_owned(), message_id.clone());
            }
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
        self.set_assistant_turn_open(true);
        self.identity_violation_reported = false;
        self.send(build_assistant_message_value(&payload));
    }

    fn set_assistant_turn_open(&mut self, open: bool) {
        self.assistant_turn_open = open;
        if self.synthetic_tool_container_id.is_some() {
            self.synthetic_tool_container_prior_assistant_turn_open = Some(open);
        }
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
        allow_synthetic_container: bool,
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
        if self.retired_tool_call_ids.contains(tool_call_id) {
            tracing::debug!(
                tool_call_id,
                tool_name,
                "Ignoring tool request for a retired tool id"
            );
            return None;
        }
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
        let opened_container =
            if allow_synthetic_container && (own_container || !self.assistant_turn_open) {
                self.ensure_assistant_turn_open(tool_call_id)
            } else {
                None
            };
        // A re-emitted request for a still-pending id is a refresh: ACP
        // agents legitimately re-title and re-argue a streaming tool call.
        // The old behavior completed the pending card as a "superseded"
        // failure before re-emitting, so every refresh shipped a failed
        // card. The frontend keys cards by tool_call_id, so re-emitting
        // the request below updates the card in place and the eventual
        // completion still pairs one-to-one with one card.
        if self.is_tool_pending(tool_call_id) {
            tracing::debug!(
                tool_call_id,
                tool_name,
                "Refreshing a pending tool request in place"
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

    fn tool_request_for_declared_response(
        &mut self,
        tool_call_id: &str,
        tool_name: &str,
        tool_type: Value,
    ) -> bool {
        let owner = self.declared_tool_owners.get(tool_call_id).cloned();
        tracing::info!(
            tool_call_id,
            owner_id = owner
                .as_ref()
                .map(|message_id| message_id.0.as_str())
                .unwrap_or("<undeclared>"),
            assistant_turn_open = self.assistant_turn_open,
            "Resolving declared provider-response tool ownership"
        );
        let Some(owner) = owner else {
            self.send(json!({
                "kind": "Error",
                "data": "Tool request was not declared by a provider response",
            }));
            return false;
        };
        if !self.terminal_stream_message_ids.contains(&owner) {
            self.send(json!({
                "kind": "Error",
                "data": "Tool request owner is not a completed provider response",
            }));
            return false;
        }
        let _ = self.tool_request(tool_call_id, tool_name, tool_type, None, false, false);
        self.is_tool_pending(tool_call_id)
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
        // A retired id completed in an earlier turn epoch. Its container's
        // message id is terminal forever, so synthesizing a request for a
        // late duplicate completion would run stream_start against a
        // terminal id and surface "duplicate terminal message id" as a
        // lone mid-chat Error card.
        if self.retired_tool_call_ids.contains(data.tool_call_id) {
            tracing::debug!(
                tool_call_id = data.tool_call_id,
                tool_name = data.tool_name,
                "Ignoring late completion for a retired tool id"
            );
            return None;
        }
        // The emitted request owns this id's identity. Providers complete
        // under drifting names — ACP re-titles tool calls mid-flight, collab
        // spawn parsers default aliases differently — so a divergent
        // completion adopts the request's name and completes the user's
        // original card. The previous supersede-and-re-request dance was a
        // silent no-op: the re-request always hit the duplicate guard, so
        // the registry kept the old name and the spawn progress emitted
        // below desynced into a user-visible identity-mismatch Error card.
        let request_name = self.pending_tool_name(data.tool_call_id).cloned();
        if let Some(request_name) = request_name.as_deref()
            && request_name != data.tool_name
        {
            tracing::warn!(
                tool_call_id = data.tool_call_id,
                request_tool_name = request_name,
                completion_tool_name = data.tool_name,
                "Tool completion name diverged from its request; completing under the request's name"
            );
        }
        let tool_name = request_name.as_deref().unwrap_or(data.tool_name);
        let mut spawn_progress = None;
        let await_progress = request_name.as_ref().and_then(|_| {
            self.emitted_tool_requests
                .get(data.tool_call_id)
                .or_else(|| self.detached_tool_requests.get(data.tool_call_id))
                .and_then(|request| {
                    terminal_await_progress_data_for_tool(
                        data.tool_call_id,
                        tool_name,
                        &request.arguments,
                        if data.success {
                            protocol::AgentControlProgressStatus::Completed
                        } else if data.error.is_some_and(|error| {
                            error.to_ascii_lowercase().contains("cancel")
                                || error.to_ascii_lowercase().contains("interrupt")
                        }) {
                            protocol::AgentControlProgressStatus::Stopped
                        } else {
                            protocol::AgentControlProgressStatus::Failed
                        },
                    )
                })
        });
        if data.success {
            match tyde_tool_result(tool_name, &data.tool_result) {
                Ok(Some(typed)) => {
                    data.tool_result = serde_json::to_value(typed).expect("serialize tool result");
                }
                Ok(None) => {}
                Err(error) => {
                    tracing::error!(
                        tool = tool_name,
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
                    tool_name,
                    &data.tool_result,
                );
            }
        }
        let mut opened_container = None;
        if request_name.is_none() {
            opened_container = self.tool_request(
                data.tool_call_id,
                tool_name,
                json!({
                    "kind": "Other",
                    "args": {
                        "synthetic": true,
                        "reason": "tool completion arrived without a pending request",
                    },
                }),
                None,
                true,
                true,
            );
        }
        let normalization_failure = merge_normalization_failures(
            self.normalization_failures.remove(data.tool_call_id),
            normalization_failure,
        );
        if let Some(progress) = spawn_progress {
            self.send_tool_progress(&progress);
        }
        if let Some(progress) = await_progress {
            self.send_tool_progress(&progress);
        }
        let success = normalization_failure.is_none() && data.success;
        let error = normalization_failure
            .as_ref()
            .map(|failure| failure.detail.clone())
            .or_else(|| data.error.map(str::to_owned));
        self.send_tool_completed_with_normalization_failure(
            data.tool_call_id,
            tool_name,
            data.tool_result,
            success,
            error.as_deref(),
            normalization_failure,
        );
        opened_container
    }

    fn send_tool_progress(&self, data: &protocol::ToolProgressData) {
        let mut data = data.clone();
        // The emitted request is the single authoritative identity for a
        // tool_call_id; producers that cache a name (sub-agent streams,
        // background tasks) can lag or guess wrong when the CLI anchors
        // task frames to a differently-named call (e.g. a SendMessage
        // continuation announced via a `task_started` frame). Stamp the
        // request's name rather than surfacing a mid-chat Error card: the
        // card punished the user for a producer bug and could trip
        // error-ends-turn handling. A divergence is still a producer bug —
        // debug builds fail loudly so tests and dev instances catch it.
        if let Some(request) = self
            .emitted_tool_requests
            .get(&data.tool_call_id)
            .or_else(|| self.detached_tool_requests.get(&data.tool_call_id))
        {
            if request.name != data.tool_name {
                tracing::error!(
                    tool_call_id = data.tool_call_id,
                    request_tool_name = request.name,
                    progress_tool_name = data.tool_name,
                    "Tool progress carried a conflicting tool identity; stamping the request's name"
                );
                debug_assert!(
                    false,
                    "tool progress identity desync for '{}': request is '{}', progress is '{}'",
                    data.tool_call_id, request.name, data.tool_name
                );
            }
            data.tool_name = request.name.clone();
        }
        let payload = match serde_json::to_value(&data) {
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
        self.declared_tool_owners.remove(tool_call_id);
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
        self.typing_active = false;
        self.reset_turn_state();
    }

    fn close(&mut self, message: &str) {
        let foreground_active = self.typing_active
            || self
                .emitted_tool_requests
                .keys()
                .any(|id| !self.completed_tool_requests.contains(id));
        if let Some(message_id) = self.current_stream_message_id.clone() {
            if self.synthetic_tool_container_id.as_ref() == Some(&message_id) {
                self.close_tool_container(message_id);
            } else {
                self.stream_end(message_id, StreamEndPayload::default());
            }
        }

        let pending = self
            .emitted_tool_requests
            .iter()
            .chain(self.detached_tool_requests.iter())
            .filter(|(id, _)| !self.completed_tool_requests.contains(*id))
            .map(|(id, request)| (id.clone(), request.name.clone()))
            .collect::<Vec<_>>();
        for (tool_call_id, tool_name) in pending {
            self.send_tool_completed(
                &tool_call_id,
                &tool_name,
                cancelled_tool_result(message),
                false,
                Some("Cancelled"),
            );
        }

        if foreground_active {
            self.send(json!({
                "kind": "OperationCancelled",
                "data": { "message": message },
            }));
            self.send(json!({
                "kind": "TypingStatusChanged",
                "data": false,
            }));
            self.typing_active = false;
        }
        self.reset_turn_state();
        self.detached_tool_requests.clear();
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
        let prior_assistant_turn_open = self.assistant_turn_open;
        self.stream_start(message_id.clone(), AgentName(&agent), model.as_deref());
        if self.stream_open && self.current_stream_message_id.as_ref() == Some(&message_id) {
            self.synthetic_tool_container_id = Some(message_id.clone());
            self.synthetic_tool_container_prior_assistant_turn_open =
                Some(prior_assistant_turn_open);
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
                self.emitted_tool_requests
                    .get(tool_call_id)
                    .or_else(|| self.detached_tool_requests.get(tool_call_id))
                    .map(|request| {
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
        self.assistant_turn_open = self
            .synthetic_tool_container_prior_assistant_turn_open
            .take()
            .unwrap_or(false);
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
        // The idle path emits only the Error card (no cancellation tail),
        // so without this WARN the incident leaves no host-log trace at all.
        tracing::warn!(
            violation = stream_identity_violation_message(violation),
            "Stream identity violation reported while emitter is idle"
        );
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
        tracing::warn!(
            violation = stream_identity_violation_message(violation),
            "Stream identity violation discarded an open stream"
        );
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
        self.typing_active = false;
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
        self.typing_active = false;
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
        self.synthetic_tool_container_prior_assistant_turn_open = None;
        self.synthetic_tool_call_ids.clear();
        self.emitted_tool_requests.clear();
        let detached_tool_requests = &self.detached_tool_requests;
        self.declared_tool_owners
            .retain(|tool_call_id, _| detached_tool_requests.contains_key(tool_call_id));
        // Retire rather than erase: terminal message ids survive this
        // reset, so completion memory must survive with them or a late
        // duplicate completion re-opens a terminal container id.
        for tool_call_id in self.completed_tool_requests.drain() {
            self.retired_tool_call_ids.insert(tool_call_id);
        }
        while self.retired_tool_call_ids.len() > RETIRED_TOOL_CALL_LEDGER_CAP {
            self.retired_tool_call_ids.shift_remove_index(0);
        }
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
