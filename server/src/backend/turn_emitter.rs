//! Shared turn-event emitter enforcing the chat protocol's ordering
//! invariants. Lifted from tycode-core's `TurnProtocol`
//! (`tycode-core/src/chat/protocol.rs`) and adapted to Tyde's
//! `mpsc::UnboundedSender<Value>` wire format.
//!
//! Every backend (Claude, Codex, Gemini, Kiro, Mock) owns exactly one
//! `Arc<TurnEmitter>` for the lifetime of the backend and routes every
//! wire event through its typed methods. There is no `send_raw` escape
//! hatch: the only way to emit is via a method defined here, so the
//! cancellation ordering spec documented on `ChatEvent` is
//! structurally enforceable — if the code compiles, each backend's
//! cancel path fires `StreamEnd` → pending `ToolExecutionCompleted`s →
//! `OperationCancelled` → `TypingStatusChanged(false)` in that order.
//!
//! Invariants:
//!   - `stream_start` sets `stream_open=true`. `stream_end` clears it
//!     and is always forwarded to the wire — Claude (and others) use
//!     StreamEnd as a turn-end sentinel even when no StreamStart was
//!     ever emitted (e.g., placeholder StreamEnd on `/compact`). The
//!     `stream_open` flag is observational: `is_stream_open()` lets
//!     backends decide whether a synthetic placeholder is needed, and
//!     `abort()` uses it to decide whether cancellation must synthesize
//!     a StreamEnd (so cancellations without an open stream don't emit
//!     a spurious StreamEnd).
//!   - `tool_request` records the id; `tool_completed` clears it. Any
//!     still-pending id at cancel time is completed as a cancellation
//!     inside `operation_cancelled`.
//!   - `operation_cancelled` walks the full cancel sequence in one
//!     place, once, per call. The state is reset after so the emitter
//!     can continue serving subsequent turns on the same backend.
//!
//! Non-`ChatEvent` wire kinds (`Settings`, `SessionStarted`,
//! `SessionsList`, `ProfilesList`, `ModuleSchemas`, `ModelsList`,
//! `ConversationCleared`, backend `Error`) also have typed methods so
//! no backend needs to reach past the emitter.

use std::collections::HashSet;

use indexmap::IndexMap;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use protocol::TaskList;

/// Sender half of the wire channel. Kept private to this module; every
/// emission must go through the typed methods below.
pub struct TurnEmitter {
    inner: std::sync::Mutex<TurnEmitterState>,
}

struct TurnEmitterState {
    tx: mpsc::UnboundedSender<Value>,
    stream_open: bool,
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
    pub usage: Option<Value>,
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
    pub content: String,
    pub reasoning: Option<Value>,
    pub tool_calls: Vec<Value>,
    pub model_info: Option<Value>,
    pub token_usage: Option<Value>,
    pub context_breakdown: Option<Value>,
    pub images: Vec<Value>,
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
        Self {
            inner: std::sync::Mutex::new(TurnEmitterState {
                tx,
                stream_open: false,
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
        self.lock().send(json!({
            "kind": "StreamDelta",
            "data": { "message_id": message_id, "text": text },
        }));
    }

    pub fn stream_reasoning_delta(&self, message_id: &str, text: &str) {
        if text.is_empty() {
            return;
        }
        self.lock().send(json!({
            "kind": "StreamReasoningDelta",
            "data": { "message_id": message_id, "text": text },
        }));
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

    // ---------- Cancellation & lifecycle ----------

    /// Full cancellation path: close open stream → complete pending
    /// tools → `OperationCancelled` → `TypingStatusChanged(false)`.
    /// Resets per-turn state so the emitter can serve the next turn.
    pub fn operation_cancelled(&self, message: &str) {
        self.lock().abort(message);
    }

    // ---------- Messages (user / assistant / system / error / warning) ----------

    pub fn user_message(&self, content: &str, images: Vec<Value>) {
        self.lock().send(json!({
            "kind": "MessageAdded",
            "data": {
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
        self.lock().send(json!({
            "kind": "MessageAdded",
            "data": {
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
        self.lock().send(json!({
            "kind": "MessageAdded",
            "data": {
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
        self.lock().send(json!({
            "kind": "MessageAdded",
            "data": {
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
        self.lock().send(json!({ "kind": "ConversationCleared" }));
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
        self.stream_open = true;
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

    fn stream_end(&mut self, payload: StreamEndPayload<'_>) {
        // Always forward: Claude uses a placeholder StreamEnd as a
        // turn-end sentinel even when no StreamStart was emitted (e.g.,
        // /compact, or a turn that completes before any content).
        self.stream_open = false;
        self.send(build_stream_end_value(&payload));
    }

    fn assistant_message(&self, payload: AssistantMessagePayload<'_>) {
        self.send(build_assistant_message_value(&payload));
    }

    fn tool_request(&mut self, tool_call_id: &str, tool_name: &str, tool_type: Value) {
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
        self.completed_tool_requests
            .insert(data.tool_call_id.to_string());
        let error_value = data
            .error
            .map(|s| Value::String(s.to_owned()))
            .unwrap_or(Value::Null);
        self.send(json!({
            "kind": "ToolExecutionCompleted",
            "data": {
                "tool_call_id": data.tool_call_id,
                "tool_name": data.tool_name,
                "tool_result": data.tool_result,
                "success": data.success,
                "error": error_value,
            },
        }));
    }

    fn abort(&mut self, cancellation_message: &str) {
        if self.stream_open {
            self.stream_end(StreamEndPayload::default());
        }

        let pending: Vec<(String, String)> = self
            .emitted_tool_requests
            .iter()
            .filter(|(id, _)| !self.completed_tool_requests.contains(*id))
            .map(|(id, name)| (id.clone(), name.clone()))
            .collect();

        for (tool_call_id, tool_name) in pending {
            self.tool_completed(ToolCompletedPayload {
                tool_call_id: &tool_call_id,
                tool_name: &tool_name,
                tool_result: json!({
                    "Error": {
                        "short_message": "Cancelled",
                        "detailed_message": "Tool execution was cancelled by user",
                    }
                }),
                success: false,
                error: Some("Cancelled by user"),
            });
        }

        self.send(json!({
            "kind": "OperationCancelled",
            "data": { "message": cancellation_message },
        }));
        self.send(json!({
            "kind": "TypingStatusChanged",
            "data": false,
        }));
        // Reset per-turn state so the next turn starts clean.
        self.emitted_tool_requests.clear();
        self.completed_tool_requests.clear();
    }
}

fn build_stream_end_value(payload: &StreamEndPayload<'_>) -> Value {
    let agent_name = payload.agent.map(|a| a.0).unwrap_or("claude");
    let model_info = payload
        .model
        .as_deref()
        .filter(|m| !m.trim().is_empty())
        .map(|m| json!({ "model": m }))
        .unwrap_or(Value::Null);
    let usage_value = payload.usage.clone().unwrap_or(Value::Null);
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

fn build_assistant_message_value(payload: &AssistantMessagePayload<'_>) -> Value {
    json!({
        "kind": "MessageAdded",
        "data": {
            "timestamp": now_ms(),
            "sender": { "Assistant": { "agent": payload.agent.0 } },
            "content": payload.content,
            "reasoning": payload.reasoning.clone().unwrap_or(Value::Null),
            "tool_calls": payload.tool_calls,
            "model_info": payload.model_info.clone().unwrap_or(Value::Null),
            "token_usage": payload.token_usage.clone().unwrap_or(Value::Null),
            "context_breakdown": payload.context_breakdown.clone().unwrap_or(Value::Null),
            "images": payload.images,
        },
    })
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

    fn recv_kinds(rx: &mut mpsc::UnboundedReceiver<Value>) -> Vec<String> {
        let mut kinds = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let Some(kind) = event.get("kind").and_then(Value::as_str) {
                kinds.push(kind.to_owned());
            }
        }
        kinds
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
        emitter.tool_request("tool-a", "Bash", json!({ "kind": "RunCommand" }));
        emitter.tool_request("tool-b", "Read", json!({ "kind": "ReadFiles" }));
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
        emitter.tool_request("tool-a", "Bash", json!({ "kind": "RunCommand" }));
        emitter.tool_completed(ToolCompletedPayload {
            tool_call_id: "tool-a",
            tool_name: "Bash",
            tool_result: json!({}),
            success: true,
            error: None,
        });
        emitter.operation_cancelled("bye");
        drop(emitter);
        let kinds = recv_kinds(&mut rx);
        assert_eq!(
            kinds,
            vec![
                "ToolRequest",
                "ToolExecutionCompleted",
                "OperationCancelled",
                "TypingStatusChanged",
            ]
        );
    }

    #[test]
    fn stream_end_without_open_stream_still_emits() {
        // Claude uses a placeholder StreamEnd as a turn-end sentinel
        // when no StreamStart was issued (e.g., /compact, or a turn
        // that ends before any content streams). The emitter must
        // forward it.
        let (tx, mut rx) = mpsc::unbounded_channel();
        let emitter = TurnEmitter::new(tx);
        emitter.stream_end(StreamEndPayload::default());
        drop(emitter);
        assert_eq!(recv_kinds(&mut rx), vec!["StreamEnd"]);
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
        emitter.tool_request("tool-a", "Bash", json!({ "kind": "RunCommand" }));
        emitter.operation_cancelled("stop");
        // Drain.
        let _ = recv_kinds(&mut rx);

        // Turn 2: a new stream, a new tool, then a second cancel.
        emitter.stream_start("msg-2", AgentName("claude"), None);
        emitter.tool_request("tool-b", "Read", json!({ "kind": "ReadFiles" }));
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
        let kinds = recv_kinds(&mut rx);
        assert_eq!(kinds, vec!["StreamDelta"]);
    }
}
