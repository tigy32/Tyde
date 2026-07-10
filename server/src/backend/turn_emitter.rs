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
//!   - `stream_start` opens a stream. If a previous stream is still
//!     open, the emitter closes it first. If a delta or StreamEnd arrives
//!     without an open stream, the emitter synthesizes a StreamStart
//!     first so the frontend never sees unpaired stream events.
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

use std::collections::HashSet;

use indexmap::IndexMap;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use protocol::{ModelRequestTokenUsage, TaskList, TokenUsageUnavailableReason};

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
    current_stream_message_id: Option<String>,
    emitted_tool_requests: IndexMap<String, String>,
    completed_tool_requests: HashSet<String>,
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
                emitted_tool_requests: IndexMap::new(),
                completed_tool_requests: HashSet::new(),
            }),
        }
    }

    // ---------- Chat stream pairing (protocol-ordered) ----------

    pub fn stream_start(&self, message_id: &str, agent: AgentName<'_>, model: Option<&str>) {
        self.lock().stream_start(message_id, agent, model);
    }

    pub fn stream_delta(&self, message_id: &str, text: &str) {
        if text.is_empty() {
            return;
        }
        self.lock().stream_delta(message_id, text);
    }

    pub fn stream_reasoning_delta(&self, message_id: &str, text: &str) {
        if text.is_empty() {
            return;
        }
        self.lock().stream_reasoning_delta(message_id, text);
    }

    pub fn stream_end(&self, payload: StreamEndPayload<'_>) {
        self.lock().stream_end(payload);
    }

    // ---------- Tool pairing (protocol-ordered) ----------

    pub fn tool_request(&self, tool_call_id: &str, tool_name: &str, tool_type: Value) {
        self.lock().tool_request(tool_call_id, tool_name, tool_type);
    }

    pub fn tool_completed(&self, data: ToolCompletedPayload<'_>) {
        self.lock().tool_completed(data);
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
        let payload = match serde_json::to_value(data) {
            Ok(value) => value,
            Err(err) => {
                tracing::warn!("failed to serialize ToolProgressData: {err}");
                return;
            }
        };
        self.lock().send(json!({
            "kind": "ToolProgress",
            "data": payload,
        }));
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

    // ---------- Misc chat events ----------

    pub fn typing_status_changed(&self, typing: bool) {
        self.lock().send(json!({
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

    fn stream_start(&mut self, message_id: &str, agent: AgentName<'_>, model: Option<&str>) {
        if self.stream_open {
            self.stream_end(StreamEndPayload::default());
        }
        self.complete_pending_tools_as_cancelled("Tool execution was cancelled before new stream");

        let message_id = normalized_id(message_id, "assistant");
        self.stream_open = true;
        self.assistant_turn_open = true;
        self.current_stream_message_id = Some(message_id.to_string());
        self.default_agent = agent.0.to_string();
        self.default_model = model.map(str::to_owned);
        let model_value = model
            .map(|m| Value::String(m.to_owned()))
            .unwrap_or(Value::Null);
        self.send(json!({
            "kind": "StreamStart",
            "data": {
                "message_id": message_id,
                "agent": agent.0,
                "model": model_value,
            },
        }));
    }

    fn stream_delta(&mut self, message_id: &str, text: &str) {
        self.ensure_stream_open(message_id);
        let message_id = normalized_id(message_id, "assistant");
        self.current_stream_message_id = Some(message_id.to_string());
        self.send(json!({
            "kind": "StreamDelta",
            "data": { "message_id": message_id, "text": text },
        }));
    }

    fn stream_reasoning_delta(&mut self, message_id: &str, text: &str) {
        self.ensure_stream_open(message_id);
        let message_id = normalized_id(message_id, "assistant");
        self.current_stream_message_id = Some(message_id.to_string());
        self.send(json!({
            "kind": "StreamReasoningDelta",
            "data": { "message_id": message_id, "text": text },
        }));
    }

    fn stream_end(&mut self, payload: StreamEndPayload<'_>) {
        if !self.stream_open {
            self.open_synthetic_stream("assistant");
        }
        let message_id = self.current_stream_message_id.clone();
        self.stream_open = false;
        self.current_stream_message_id = None;
        self.send(build_stream_end_value(
            &payload,
            &self.default_agent,
            message_id.as_deref(),
        ));
    }

    fn assistant_message(&mut self, payload: AssistantMessagePayload<'_>) {
        self.complete_pending_tools_as_cancelled(
            "Tool execution was cancelled before assistant message",
        );
        self.assistant_turn_open = true;
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

    fn tool_request(&mut self, tool_call_id: &str, tool_name: &str, tool_type: Value) {
        if !self.assistant_turn_open {
            self.ensure_assistant_turn_open(tool_call_id);
        }
        if self.is_tool_pending(tool_call_id) {
            let existing_name = self
                .emitted_tool_requests
                .get(tool_call_id)
                .cloned()
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
        self.emitted_tool_requests
            .insert(tool_call_id.to_string(), tool_name.to_string());
        self.send(json!({
            "kind": "ToolRequest",
            "data": {
                "tool_call_id": tool_call_id,
                "tool_name": tool_name,
                "tool_type": tool_type,
            },
        }));
    }

    fn tool_completed(&mut self, data: ToolCompletedPayload<'_>) {
        match self.pending_tool_name(data.tool_call_id).cloned() {
            Some(expected_name) if expected_name != data.tool_name => {
                self.send_tool_completed(
                    data.tool_call_id,
                    &expected_name,
                    cancelled_tool_result("Tool completion name mismatch was superseded"),
                    false,
                    Some("Tool completion name mismatch was superseded"),
                );
                self.tool_request(
                    data.tool_call_id,
                    data.tool_name,
                    json!({
                        "kind": "Other",
                        "args": {
                            "synthetic": true,
                            "reason": "tool completion arrived without a matching request",
                        },
                    }),
                );
            }
            Some(_) => {}
            None => {
                self.tool_request(
                    data.tool_call_id,
                    data.tool_name,
                    json!({
                        "kind": "Other",
                        "args": {
                            "synthetic": true,
                            "reason": "tool completion arrived without a pending request",
                        },
                    }),
                );
            }
        }
        self.send_tool_completed(
            data.tool_call_id,
            data.tool_name,
            data.tool_result,
            data.success,
            data.error,
        );
    }

    fn send_tool_completed(
        &mut self,
        tool_call_id: &str,
        tool_name: &str,
        tool_result: Value,
        success: bool,
        error: Option<&str>,
    ) {
        self.completed_tool_requests
            .insert(tool_call_id.to_string());
        let error_value = error
            .map(|s| Value::String(s.to_owned()))
            .unwrap_or(Value::Null);
        self.send(json!({
            "kind": "ToolExecutionCompleted",
            "data": {
                "tool_call_id": tool_call_id,
                "tool_name": tool_name,
                "tool_result": tool_result,
                "success": success,
                "error": error_value,
            },
        }));
    }

    fn abort(&mut self, cancellation_message: &str) {
        if self.stream_open {
            self.stream_end(StreamEndPayload::default());
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

    fn ensure_stream_open(&mut self, message_id: &str) {
        if !self.stream_open {
            self.open_synthetic_stream(message_id);
        }
    }

    fn ensure_assistant_turn_open(&mut self, message_id: &str) {
        if !self.assistant_turn_open {
            self.open_synthetic_stream(message_id);
        }
    }

    fn open_synthetic_stream(&mut self, message_id: &str) {
        let agent = self.default_agent.clone();
        let model = self.default_model.clone();
        self.stream_start(message_id, AgentName(&agent), model.as_deref());
    }

    fn complete_pending_tools_as_cancelled(&mut self, detailed_message: &str) {
        let pending: Vec<(String, String)> = self
            .emitted_tool_requests
            .iter()
            .filter(|(id, _)| !self.completed_tool_requests.contains(*id))
            .map(|(id, name)| (id.clone(), name.clone()))
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

    fn pending_tool_name(&self, tool_call_id: &str) -> Option<&String> {
        self.emitted_tool_requests
            .get(tool_call_id)
            .filter(|_| !self.completed_tool_requests.contains(tool_call_id))
    }

    fn is_tool_pending(&self, tool_call_id: &str) -> bool {
        self.pending_tool_name(tool_call_id).is_some()
    }

    fn reset_turn_state(&mut self) {
        self.stream_open = false;
        self.assistant_turn_open = false;
        self.current_stream_message_id = None;
        self.emitted_tool_requests.clear();
        self.completed_tool_requests.clear();
    }
}

fn build_stream_end_value(
    payload: &StreamEndPayload<'_>,
    default_agent: &str,
    message_id: Option<&str>,
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
                "images": [],
            }
        },
    })
}

fn normalized_id<'a>(id: &'a str, fallback: &'a str) -> &'a str {
    if id.trim().is_empty() { fallback } else { id }
}

fn cancelled_tool_result(detailed_message: &str) -> Value {
    json!({
        "kind": "Error",
        "short_message": "Cancelled",
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
    json!({
        "request": token_usage_scope_value(request_usage),
        "turn": token_usage_scope_value(turn_usage),
        "cumulative": token_usage_scope_value(cumulative_usage),
    })
}

fn token_usage_scope_value(usage: Option<Value>) -> Value {
    match usage {
        Some(usage) => json!({ "kind": "known", "usage": usage }),
        None => {
            token_usage_unavailable_scope_value(TokenUsageUnavailableReason::BackendDidNotReport)
        }
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
        NewAgentPayload, PROTOCOL_VERSION, ProtocolValidator, StreamPath, TeamPresetCatalog,
        Version, WelcomePayload,
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
    fn stream_end_without_open_stream_synthesizes_stream_start() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let emitter = TurnEmitter::new(tx);
        emitter.stream_end(StreamEndPayload::default());
        drop(emitter);
        let events = recv_events(&mut rx);
        assert_protocol_valid(&events);
        assert_eq!(event_kinds(&events), vec!["StreamStart", "StreamEnd"]);
    }

    #[test]
    fn stream_end_carries_active_stream_message_id() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let emitter = TurnEmitter::new_for_agent(tx, AgentName("codex"));
        emitter.stream_start("turn-1", AgentName("codex"), Some("gpt-5-codex"));
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
        emitter.stream_delta("msg-1", "");
        emitter.stream_delta("msg-1", "hi");
        drop(emitter);
        let events = recv_events(&mut rx);
        assert_protocol_valid(&events);
        assert_eq!(event_kinds(&events), vec!["StreamStart", "StreamDelta"]);
    }

    #[test]
    fn reasoning_delta_without_open_stream_synthesizes_stream_start() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let emitter = TurnEmitter::new_for_agent(tx, AgentName("codex"));
        emitter.stream_reasoning_delta("reason-1", "thinking");
        drop(emitter);
        let events = recv_events(&mut rx);
        assert_protocol_valid(&events);
        assert_eq!(
            event_kinds(&events),
            vec!["StreamStart", "StreamReasoningDelta"]
        );
        assert_eq!(
            events[0]
                .get("data")
                .and_then(|data| data.get("agent"))
                .and_then(Value::as_str),
            Some("codex")
        );
    }

    #[test]
    fn second_stream_start_closes_previous_stream_first() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let emitter = TurnEmitter::new_for_agent(tx, AgentName("codex"));
        emitter.stream_start("msg-1", AgentName("codex"), Some("model-a"));
        emitter.stream_start("msg-2", AgentName("codex"), Some("model-a"));
        drop(emitter);
        let events = recv_events(&mut rx);
        assert_protocol_valid(&events);
        assert_eq!(
            event_kinds(&events),
            vec!["StreamStart", "StreamEnd", "StreamStart"]
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
