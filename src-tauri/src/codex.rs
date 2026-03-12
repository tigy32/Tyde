use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine as _;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{mpsc, oneshot, Mutex};

use crate::backend::{SessionCommand, StartupMcpServer, StartupMcpTransport};
use crate::subprocess::ImageAttachment;

const CODEX_REQUEST_TIMEOUT: Duration = Duration::from_secs(45);
const CODEX_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
const CODEX_AGENT_NAME: &str = "codex";
const CODEX_ESTIMATED_CONTEXT_WINDOW: u64 = 256_000;
const CODEX_ESTIMATED_BYTES_PER_TOKEN: u64 = 4;
const CODEX_MIN_SYSTEM_PROMPT_BYTES: u64 = 1_024;
const CODEX_FORCED_APPROVAL_POLICY: &str = "never";
const CODEX_ENABLE_EXPERIMENTAL_RAW_EVENTS: bool = true;
const CODEX_REASONING_SUMMARY_LEVEL: &str = "detailed";

#[derive(Clone)]
pub struct CodexCommandHandle {
    inner: Arc<CodexInner>,
}

impl CodexCommandHandle {
    pub async fn execute(&self, command: SessionCommand) -> Result<(), String> {
        self.inner.execute(command).await
    }
}

pub struct CodexSession {
    inner: Arc<CodexInner>,
}

impl CodexSession {
    pub async fn spawn(
        workspace_roots: &[String],
        ssh_host: Option<String>,
        startup_mcp_servers: &[StartupMcpServer],
    ) -> Result<(Self, mpsc::UnboundedReceiver<Value>), String> {
        Self::spawn_with_mode(workspace_roots, false, ssh_host, startup_mcp_servers).await
    }

    pub async fn spawn_ephemeral(
        workspace_roots: &[String],
        ssh_host: Option<String>,
        startup_mcp_servers: &[StartupMcpServer],
    ) -> Result<(Self, mpsc::UnboundedReceiver<Value>), String> {
        Self::spawn_with_mode(workspace_roots, true, ssh_host, startup_mcp_servers).await
    }

    pub async fn spawn_admin(
        workspace_roots: &[String],
        ssh_host: Option<String>,
        startup_mcp_servers: &[StartupMcpServer],
    ) -> Result<(Self, mpsc::UnboundedReceiver<Value>), String> {
        Self::spawn_with_mode(workspace_roots, true, ssh_host, startup_mcp_servers).await
    }

    async fn spawn_with_mode(
        workspace_roots: &[String],
        ephemeral: bool,
        ssh_host: Option<String>,
        startup_mcp_servers: &[StartupMcpServer],
    ) -> Result<(Self, mpsc::UnboundedReceiver<Value>), String> {
        let (rpc, inbound_rx) = CodexRpc::spawn(ssh_host.as_deref(), startup_mcp_servers)?;

        rpc.request(
            "initialize",
            json!({
                "clientInfo": {
                    "name": "tyde",
                    "title": Value::Null,
                    "version": "0.1"
                },
                "capabilities": {
                    "experimentalApi": true
                }
            }),
        )
        .await?;

        let cwd = if ssh_host.is_some() {
            // For remote sessions, extract the remote path (host already stripped)
            let parsed = crate::remote::parse_remote_workspace_roots(workspace_roots)?
                .ok_or("Expected remote workspace roots for SSH session")?;
            parsed
                .1
                .into_iter()
                .next()
                .ok_or("No remote workspace root found")?
        } else {
            pick_workspace_root(workspace_roots)?
        };

        let thread_started = rpc
            .request(
                "thread/start",
                json!({
                    "cwd": cwd,
                    "sandbox": "workspace-write",
                    "approvalPolicy": CODEX_FORCED_APPROVAL_POLICY,
                    "ephemeral": ephemeral,
                    "experimentalRawEvents": CODEX_ENABLE_EXPERIMENTAL_RAW_EVENTS,
                    "persistExtendedHistory": false
                }),
            )
            .await?;

        let thread_id = thread_started
            .get("thread")
            .and_then(|t| t.get("id"))
            .and_then(Value::as_str)
            .ok_or("Codex thread/start response missing thread.id")?
            .to_string();

        let model = thread_started
            .get("model")
            .and_then(Value::as_str)
            .map(|s| s.to_string());

        let (event_tx, event_rx) = mpsc::unbounded_channel();

        let inner = Arc::new(CodexInner {
            rpc,
            event_tx,
            state: Mutex::new(CodexState {
                thread_id,
                model,
                reasoning_effort: Some("xhigh".to_string()),
                approval_policy: None,
                active_turn_id: None,
                active_stream: None,
                token_usage_by_turn: HashMap::new(),
                turn_context_by_turn: HashMap::new(),
                file_change_call_ids: HashMap::new(),
                pending_request: None,
                pending_user_input_bytes: 0,
                conversation_bytes_total: 0,
            }),
        });

        let forward_inner = Arc::clone(&inner);
        tokio::spawn(async move {
            let mut rx = inbound_rx;
            while let Some(msg) = rx.recv().await {
                forward_inner.handle_inbound(msg).await;
            }
        });

        Ok((Self { inner }, event_rx))
    }

    pub fn command_handle(&self) -> CodexCommandHandle {
        CodexCommandHandle {
            inner: Arc::clone(&self.inner),
        }
    }

    pub async fn session_id(&self) -> Option<String> {
        Some(self.inner.state.lock().await.thread_id.clone())
    }

    pub async fn shutdown(self) {
        self.inner.rpc.shutdown().await;
    }
}

#[derive(Clone)]
struct PendingRequest {
    request_id: Value,
    tool_call_id: String,
    kind: PendingRequestKind,
}

#[derive(Clone)]
enum PendingRequestKind {
    CommandApproval,
    FileChangeApproval,
    ExecCommandApproval,
    ApplyPatchApproval,
    UserInput { questions: Vec<String> },
}

#[derive(Clone, Default)]
struct ActiveStreamState {
    turn_id: String,
    message_id: String,
    text: String,
    reasoning: String,
}

#[derive(Clone, Default)]
struct TurnContextEstimate {
    conversation_history_bytes: u64,
    tool_io_bytes: u64,
    reasoning_bytes: u64,
}

struct CodexState {
    thread_id: String,
    model: Option<String>,
    reasoning_effort: Option<String>,
    approval_policy: Option<String>,
    active_turn_id: Option<String>,
    active_stream: Option<ActiveStreamState>,
    token_usage_by_turn: HashMap<String, Value>,
    turn_context_by_turn: HashMap<String, TurnContextEstimate>,
    file_change_call_ids: HashMap<String, Vec<String>>,
    pending_request: Option<PendingRequest>,
    pending_user_input_bytes: u64,
    conversation_bytes_total: u64,
}

struct CodexInner {
    rpc: CodexRpc,
    event_tx: mpsc::UnboundedSender<Value>,
    state: Mutex<CodexState>,
}

impl CodexInner {
    async fn execute(&self, command: SessionCommand) -> Result<(), String> {
        match command {
            SessionCommand::SendMessage { message, images } => {
                self.emit_user_message_added(&message, images.as_deref());
                // UI contract: show typing immediately when a user turn is submitted,
                // without waiting for Codex to acknowledge turn/started.
                self.emit_event(json!({ "kind": "TypingStatusChanged", "data": true }));

                if self.respond_pending_request(&message).await? {
                    return Ok(());
                }

                let (thread_id, model_override, effort_override, approval_policy_override) = {
                    let mut state = self.state.lock().await;
                    state.pending_user_input_bytes = message.len() as u64;
                    (
                        state.thread_id.clone(),
                        state.model.clone(),
                        state.reasoning_effort.clone(),
                        state.approval_policy.clone(),
                    )
                };

                let mut input_items = vec![json!({
                    "type": "text",
                    "text": message,
                    "text_elements": []
                })];

                if let Some(imgs) = images {
                    for image in imgs {
                        let path = persist_temp_image(&image).await?;
                        input_items.push(json!({
                            "type": "localImage",
                            "path": path
                        }));
                    }
                }

                let mut params = json!({
                    "threadId": thread_id,
                    "input": input_items
                });

                if let Some(model) = model_override {
                    params["model"] = Value::String(model);
                }
                if let Some(effort) = effort_override {
                    params["effort"] = Value::String(effort);
                }
                params["summary"] = Value::String(CODEX_REASONING_SUMMARY_LEVEL.to_string());
                let approval_policy = approval_policy_override
                    .unwrap_or_else(|| CODEX_FORCED_APPROVAL_POLICY.to_string());
                params["approvalPolicy"] = Value::String(approval_policy);

                if let Err(err) = self.rpc.request("turn/start", params).await {
                    self.emit_event(json!({ "kind": "TypingStatusChanged", "data": false }));
                    return Err(err);
                }
                Ok(())
            }
            SessionCommand::CancelConversation => {
                let (thread_id, turn_id_opt) = {
                    let state = self.state.lock().await;
                    (state.thread_id.clone(), state.active_turn_id.clone())
                };
                let Some(turn_id) = turn_id_opt else {
                    return Ok(());
                };
                let _ = self
                    .rpc
                    .request(
                        "turn/interrupt",
                        json!({
                            "threadId": thread_id,
                            "turnId": turn_id
                        }),
                    )
                    .await?;
                Ok(())
            }
            SessionCommand::GetSettings => {
                // Phase 6 handles config/settings parity. Keep non-failing no-op for now.
                Ok(())
            }
            SessionCommand::ListSessions => self.list_sessions().await,
            SessionCommand::ResumeSession { session_id } => self.resume_session(session_id).await,
            SessionCommand::DeleteSession { session_id } => self.delete_session(session_id).await,
            SessionCommand::ListProfiles => {
                // Phase 6 handles profiles parity.
                Ok(())
            }
            SessionCommand::SwitchProfile { profile_name: _ } => {
                // Phase 6 handles profile switching parity.
                Ok(())
            }
            SessionCommand::GetModuleSchemas => {
                // Phase 6 handles module schema parity.
                Ok(())
            }
            SessionCommand::ListModels => self.list_models().await,
            SessionCommand::UpdateSettings {
                settings,
                persist: _,
            } => {
                if let Some(obj) = settings.as_object() {
                    let mut state = self.state.lock().await;

                    if let Some(model_value) = obj.get("model") {
                        if model_value.is_null() {
                            state.model = None;
                        } else if let Some(model) = model_value.as_str() {
                            let normalized = model.trim();
                            state.model = if normalized.is_empty() {
                                None
                            } else {
                                Some(normalized.to_string())
                            };
                        }
                    }

                    if let Some(effort_value) = obj
                        .get("reasoning_effort")
                        .or_else(|| obj.get("reasoningEffort"))
                    {
                        if effort_value.is_null() {
                            state.reasoning_effort = None;
                        } else if let Some(raw) = effort_value.as_str() {
                            state.reasoning_effort = normalize_reasoning_effort(raw);
                        }
                    }

                    if obj.contains_key("approval_policy") || obj.contains_key("approvalPolicy") {
                        state.approval_policy = Some(CODEX_FORCED_APPROVAL_POLICY.to_string());
                    }
                }
                Ok(())
            }
        }
    }

    async fn list_sessions(&self) -> Result<(), String> {
        let mut cursor: Option<String> = None;
        let mut sessions: Vec<Value> = Vec::new();

        for _ in 0..20 {
            let mut params = json!({ "limit": 100 });
            if let Some(cur) = cursor.as_ref() {
                params["cursor"] = Value::String(cur.clone());
            }

            let response = self.rpc.request("thread/list", params).await?;
            let page = response
                .get("data")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();

            if page.is_empty() {
                break;
            }

            for thread in page {
                if let Some(metadata) = codex_thread_to_session_metadata(&thread) {
                    sessions.push(metadata);
                }
            }

            cursor = response
                .get("nextCursor")
                .and_then(Value::as_str)
                .map(|s| s.to_string());

            if cursor.is_none() || sessions.len() >= 1000 {
                break;
            }
        }

        self.emit_event(json!({
            "kind": "SessionsList",
            "data": {
                "sessions": sessions
            }
        }));
        Ok(())
    }

    async fn resume_session(&self, session_id: String) -> Result<(), String> {
        let response = self
            .rpc
            .request(
                "thread/resume",
                json!({
                    "threadId": session_id,
                    "experimentalRawEvents": CODEX_ENABLE_EXPERIMENTAL_RAW_EVENTS
                }),
            )
            .await?;

        let thread = response
            .get("thread")
            .ok_or("Codex thread/resume response missing thread")?;
        let resumed_thread_id = thread
            .get("id")
            .and_then(Value::as_str)
            .ok_or("Codex thread/resume response missing thread.id")?
            .to_string();
        let resumed_model = response
            .get("model")
            .and_then(Value::as_str)
            .map(|s| s.to_string());
        let turns = thread
            .get("turns")
            .and_then(Value::as_array)
            .cloned()
            .ok_or_else(|| "Codex resume response missing 'turns' array".to_string())?;

        {
            let mut state = self.state.lock().await;
            state.thread_id = resumed_thread_id;
            if let Some(model) = resumed_model.clone() {
                state.model = Some(model);
            }
            state.active_turn_id = None;
            state.active_stream = None;
            state.token_usage_by_turn.clear();
            state.turn_context_by_turn.clear();
            state.file_change_call_ids.clear();
            state.pending_request = None;
            state.pending_user_input_bytes = 0;
            state.conversation_bytes_total = 0;
        }

        self.emit_event(json!({ "kind": "ConversationCleared" }));
        self.emit_event(json!({ "kind": "TypingStatusChanged", "data": false }));

        let model = resumed_model.unwrap_or_else(|| "codex".to_string());
        let restored_bytes = self.emit_resumed_thread_history(&turns, &model).await;
        let mut state = self.state.lock().await;
        state.conversation_bytes_total = restored_bytes;

        Ok(())
    }

    async fn delete_session(&self, session_id: String) -> Result<(), String> {
        match self
            .rpc
            .request(
                "thread/archive",
                json!({
                    "threadId": session_id
                }),
            )
            .await
        {
            Ok(_) => Ok(()),
            Err(err) => {
                let normalized = err.to_ascii_lowercase();
                if normalized.contains("no rollout found")
                    || normalized.contains("thread not found")
                    || normalized.contains("not found")
                {
                    return Ok(());
                }
                Err(err)
            }
        }
    }

    async fn list_models(&self) -> Result<(), String> {
        let response = self
            .rpc
            .request("model/list", json!({ "includeHidden": false }))
            .await?;

        let raw_models = response
            .get("data")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        let models: Vec<Value> = raw_models
            .iter()
            .filter_map(|m| {
                let id = m
                    .get("model")
                    .or_else(|| m.get("id"))
                    .and_then(Value::as_str)?;
                let display_name = m.get("displayName").and_then(Value::as_str).unwrap_or(id);
                let is_default = m.get("isDefault").and_then(Value::as_bool).unwrap_or(false);
                Some(json!({
                    "id": id,
                    "displayName": display_name,
                    "isDefault": is_default,
                }))
            })
            .collect();

        self.emit_event(json!({
            "kind": "ModelsList",
            "data": {
                "models": models
            }
        }));
        Ok(())
    }

    async fn emit_resumed_thread_history(&self, turns: &[Value], model: &str) -> u64 {
        let mut total_bytes = 0u64;

        for turn in turns {
            let Some(items) = turn.get("items").and_then(Value::as_array) else {
                continue;
            };

            for item in items {
                let item_type = item.get("type").and_then(Value::as_str).unwrap_or_default();

                match item_type {
                    "userMessage" => {
                        let text = extract_codex_item_text(item);
                        if text.trim().is_empty() {
                            continue;
                        }
                        total_bytes = total_bytes.saturating_add(text.len() as u64);
                        self.emit_event(json!({
                            "kind": "MessageAdded",
                            "data": {
                                "timestamp": unix_now_ms(),
                                "sender": "User",
                                "content": text,
                                "tool_calls": [],
                                "images": []
                            }
                        }));
                    }
                    "agentMessage" => {
                        let text = extract_codex_item_text(item);
                        if text.trim().is_empty() {
                            continue;
                        }
                        let reasoning = extract_codex_item_reasoning(item);
                        total_bytes = total_bytes.saturating_add(text.len() as u64);
                        self.emit_event(json!({
                            "kind": "MessageAdded",
                            "data": {
                                "timestamp": unix_now_ms(),
                                "sender": { "Assistant": { "agent": CODEX_AGENT_NAME } },
                                "content": text,
                                "reasoning": reasoning.map(|summary| json!({ "text": summary })).unwrap_or(Value::Null),
                                "tool_calls": [],
                                "model_info": { "model": model },
                                "images": []
                            }
                        }));
                    }
                    _ => {}
                }
            }
        }

        total_bytes
    }

    async fn respond_pending_request(&self, message: &str) -> Result<bool, String> {
        let pending = {
            let mut state = self.state.lock().await;
            state.pending_request.take()
        };

        let Some(pending) = pending else {
            return Ok(false);
        };

        match pending.kind {
            PendingRequestKind::CommandApproval => {
                let decision = parse_approval_decision(message);
                self.rpc
                    .respond(
                        pending.request_id.clone(),
                        json!({
                            "decision": decision
                        }),
                    )
                    .await?;
                self.emit_tool_execution_completed(
                    &pending.tool_call_id,
                    "approval",
                    true,
                    json!({"kind": "Other", "result": {"decision": decision}}),
                    None,
                );
            }
            PendingRequestKind::FileChangeApproval => {
                let decision = parse_approval_decision(message);
                self.rpc
                    .respond(
                        pending.request_id.clone(),
                        json!({
                            "decision": decision
                        }),
                    )
                    .await?;
                self.emit_tool_execution_completed(
                    &pending.tool_call_id,
                    "file_change_approval",
                    true,
                    json!({"kind": "Other", "result": {"decision": decision}}),
                    None,
                );
            }
            PendingRequestKind::ExecCommandApproval => {
                let decision = parse_review_decision(message);
                self.rpc
                    .respond(
                        pending.request_id.clone(),
                        json!({
                            "decision": decision
                        }),
                    )
                    .await?;
                self.emit_tool_execution_completed(
                    &pending.tool_call_id,
                    "exec_command_approval",
                    true,
                    json!({"kind": "Other", "result": {"decision": decision}}),
                    None,
                );
            }
            PendingRequestKind::ApplyPatchApproval => {
                let decision = parse_review_decision(message);
                self.rpc
                    .respond(
                        pending.request_id.clone(),
                        json!({
                            "decision": decision
                        }),
                    )
                    .await?;
                self.emit_tool_execution_completed(
                    &pending.tool_call_id,
                    "apply_patch_approval",
                    true,
                    json!({"kind": "Other", "result": {"decision": decision}}),
                    None,
                );
            }
            PendingRequestKind::UserInput { questions } => {
                let normalized = if message.trim().is_empty() {
                    String::new()
                } else {
                    message.trim().to_string()
                };
                let mut answers = serde_json::Map::new();
                for q in &questions {
                    answers.insert(q.clone(), json!({ "answers": [normalized] }));
                }
                self.rpc
                    .respond(
                        pending.request_id.clone(),
                        json!({
                            "answers": answers
                        }),
                    )
                    .await?;
                self.emit_tool_execution_completed(
                    &pending.tool_call_id,
                    "ask_user_question",
                    true,
                    json!({"kind": "Other", "result": {"answered": true}}),
                    None,
                );
            }
        }

        Ok(true)
    }

    async fn handle_inbound(&self, inbound: CodexInbound) {
        match inbound {
            CodexInbound::Stderr(line) => {
                self.emit_event(json!({
                    "kind": "SubprocessStderr",
                    "data": line
                }));
            }
            CodexInbound::Closed { exit_code } => {
                self.emit_event(json!({
                    "kind": "SubprocessExit",
                    "data": { "exit_code": exit_code }
                }));
            }
            CodexInbound::Notification { method, params } => {
                if method.starts_with("codex/event/") {
                    self.handle_legacy_codex_event(&method, &params).await;
                    return;
                }
                self.handle_notification(&method, &params).await;
            }
            CodexInbound::ServerRequest { id, method, params } => {
                self.handle_server_request(id, &method, &params).await;
            }
        }
    }

    async fn handle_notification(&self, method: &str, params: &Value) {
        match method {
            "turn/started" => {
                let turn_id = params
                    .get("turn")
                    .and_then(|v| v.get("id"))
                    .and_then(Value::as_str)
                    .unwrap_or("turn")
                    .to_string();
                let model = {
                    let mut state = self.state.lock().await;
                    state.active_turn_id = Some(turn_id.clone());
                    state.active_stream = Some(ActiveStreamState {
                        turn_id: turn_id.clone(),
                        message_id: turn_id.clone(),
                        text: String::new(),
                        reasoning: String::new(),
                    });
                    let pending_user_input = state.pending_user_input_bytes;
                    state.pending_user_input_bytes = 0;
                    state.conversation_bytes_total = state
                        .conversation_bytes_total
                        .saturating_add(pending_user_input);
                    let history_bytes = state.conversation_bytes_total;
                    state.turn_context_by_turn.insert(
                        turn_id.clone(),
                        TurnContextEstimate {
                            conversation_history_bytes: history_bytes,
                            ..TurnContextEstimate::default()
                        },
                    );
                    state.model.clone().unwrap_or_else(|| "codex".to_string())
                };
                self.emit_event(json!({ "kind": "TypingStatusChanged", "data": true }));
                self.emit_event(json!({
                    "kind": "StreamStart",
                    "data": {
                        "message_id": turn_id,
                        "agent": CODEX_AGENT_NAME,
                        "model": model
                    }
                }));
            }
            "item/agentMessage/delta" => {
                let delta = params
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let message_id = params
                    .get("itemId")
                    .and_then(Value::as_str)
                    .unwrap_or("assistant")
                    .to_string();
                if delta.is_empty() {
                    return;
                }
                {
                    let mut state = self.state.lock().await;
                    if let Some(stream) = state.active_stream.as_mut() {
                        stream.message_id = message_id.clone();
                        stream.text.push_str(&delta);
                    }
                }
                self.emit_event(json!({
                    "kind": "StreamDelta",
                    "data": {
                        "message_id": message_id,
                        "text": delta
                    }
                }));
            }
            reasoning_method if is_reasoning_notification_method(reasoning_method) => {
                let Some(delta) = extract_codex_reasoning_delta_text(params) else {
                    return;
                };
                self.emit_reasoning_delta(params, delta).await;
            }
            "item/started" => {
                self.handle_item_started(params).await;
            }
            "item/completed" => {
                self.handle_item_completed(params).await;
            }
            "turn/plan/updated" => {
                self.handle_plan_update(params);
            }
            "thread/tokenUsage/updated" => {
                if let Some((turn_id, token_usage)) = extract_turn_token_usage(params) {
                    let mut state = self.state.lock().await;
                    state.token_usage_by_turn.insert(turn_id, token_usage);
                }
            }
            "model/rerouted" => {
                if let Some(model) = params.get("toModel").and_then(Value::as_str) {
                    let mut state = self.state.lock().await;
                    state.model = Some(model.to_string());
                }
            }
            "turn/completed" => {
                self.handle_turn_completed(params).await;
            }
            "error" => {
                self.handle_error_notification(params).await;
            }
            _ => {}
        }
    }

    async fn handle_legacy_codex_event(&self, method: &str, params: &Value) {
        let Some(delta) = extract_reasoning_delta_from_legacy_codex_event(method, params) else {
            return;
        };
        self.emit_reasoning_delta(params, delta).await;
    }

    async fn emit_reasoning_delta(&self, params: &Value, delta: String) {
        let event = {
            let mut state = self.state.lock().await;
            apply_reasoning_delta_to_state(&mut state, params, &delta)
        };
        if let Some(event) = event {
            self.emit_event(event);
        }
    }

    async fn handle_error_notification(&self, params: &Value) {
        let message = params
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("Codex error")
            .to_string();
        let terminal = {
            let state = self.state.lock().await;
            is_terminal_codex_error_notification(&state, params)
        };

        if terminal {
            self.emit_event(json!({ "kind": "Error", "data": message }));
            self.emit_event(json!({ "kind": "TypingStatusChanged", "data": false }));
            return;
        }

        self.emit_event(json!({
            "kind": "SubprocessStderr",
            "data": format!("Codex warning: {message}")
        }));
    }

    async fn handle_server_request(&self, id: Value, method: &str, params: &Value) {
        match method {
            "item/commandExecution/requestApproval" => {
                let item_id = params
                    .get("itemId")
                    .and_then(Value::as_str)
                    .unwrap_or("approval")
                    .to_string();
                let tool_call_id = format!("approval-{item_id}");
                let question = params
                    .get("reason")
                    .and_then(Value::as_str)
                    .map(|s| s.to_string())
                    .or_else(|| {
                        params
                            .get("command")
                            .and_then(Value::as_str)
                            .map(|cmd| format!("Approve command: {cmd}"))
                    })
                    .unwrap_or_else(|| "Approve pending command?".to_string());

                {
                    let mut state = self.state.lock().await;
                    state.pending_request = Some(PendingRequest {
                        request_id: id,
                        tool_call_id: tool_call_id.clone(),
                        kind: PendingRequestKind::CommandApproval,
                    });
                }

                self.emit_event(json!({ "kind": "TypingStatusChanged", "data": false }));
                self.emit_event(json!({
                    "kind": "ToolRequest",
                    "data": {
                        "tool_call_id": tool_call_id,
                        "tool_name": "ask_user_question",
                        "tool_type": {
                            "kind": "Other",
                            "args": {
                                "question": question,
                                "type": "command_approval"
                            }
                        }
                    }
                }));
            }
            "item/fileChange/requestApproval" => {
                let item_id = params
                    .get("itemId")
                    .and_then(Value::as_str)
                    .unwrap_or("file-approval")
                    .to_string();
                let tool_call_id = format!("file-approval-{item_id}");
                let question = params
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("Approve pending file changes?")
                    .to_string();

                {
                    let mut state = self.state.lock().await;
                    state.pending_request = Some(PendingRequest {
                        request_id: id,
                        tool_call_id: tool_call_id.clone(),
                        kind: PendingRequestKind::FileChangeApproval,
                    });
                }

                self.emit_event(json!({ "kind": "TypingStatusChanged", "data": false }));
                self.emit_event(json!({
                    "kind": "ToolRequest",
                    "data": {
                        "tool_call_id": tool_call_id,
                        "tool_name": "ask_user_question",
                        "tool_type": {
                            "kind": "Other",
                            "args": {
                                "question": question,
                                "type": "file_change_approval"
                            }
                        }
                    }
                }));
            }
            "execCommandApproval" => {
                let call_id = params
                    .get("callId")
                    .and_then(Value::as_str)
                    .unwrap_or("exec-approval")
                    .to_string();
                let tool_call_id = format!("exec-approval-{call_id}");
                let command_text = params
                    .get("command")
                    .and_then(Value::as_array)
                    .map(|parts| {
                        parts
                            .iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
                    .unwrap_or_default();
                let question = params
                    .get("reason")
                    .and_then(Value::as_str)
                    .map(|s| s.to_string())
                    .or_else(|| {
                        if command_text.is_empty() {
                            None
                        } else {
                            Some(format!("Approve command: {command_text}"))
                        }
                    })
                    .unwrap_or_else(|| "Approve pending command?".to_string());

                {
                    let mut state = self.state.lock().await;
                    state.pending_request = Some(PendingRequest {
                        request_id: id,
                        tool_call_id: tool_call_id.clone(),
                        kind: PendingRequestKind::ExecCommandApproval,
                    });
                }

                self.emit_event(json!({ "kind": "TypingStatusChanged", "data": false }));
                self.emit_event(json!({
                    "kind": "ToolRequest",
                    "data": {
                        "tool_call_id": tool_call_id,
                        "tool_name": "ask_user_question",
                        "tool_type": {
                            "kind": "Other",
                            "args": {
                                "question": question,
                                "type": "command_approval"
                            }
                        }
                    }
                }));
            }
            "applyPatchApproval" => {
                let call_id = params
                    .get("callId")
                    .and_then(Value::as_str)
                    .unwrap_or("patch-approval")
                    .to_string();
                let tool_call_id = format!("patch-approval-{call_id}");
                let question = params
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("Approve pending file changes?")
                    .to_string();

                {
                    let mut state = self.state.lock().await;
                    state.pending_request = Some(PendingRequest {
                        request_id: id,
                        tool_call_id: tool_call_id.clone(),
                        kind: PendingRequestKind::ApplyPatchApproval,
                    });
                }

                self.emit_event(json!({ "kind": "TypingStatusChanged", "data": false }));
                self.emit_event(json!({
                    "kind": "ToolRequest",
                    "data": {
                        "tool_call_id": tool_call_id,
                        "tool_name": "ask_user_question",
                        "tool_type": {
                            "kind": "Other",
                            "args": {
                                "question": question,
                                "type": "file_change_approval"
                            }
                        }
                    }
                }));
            }
            "item/tool/requestUserInput" => {
                let item_id = params
                    .get("itemId")
                    .and_then(Value::as_str)
                    .unwrap_or("request-user-input")
                    .to_string();
                let tool_call_id = format!("request-user-input-{item_id}");
                let questions = params
                    .get("questions")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let question_ids = questions
                    .iter()
                    .filter_map(|q| q.get("id").and_then(Value::as_str).map(|s| s.to_string()))
                    .collect::<Vec<_>>();

                {
                    let mut state = self.state.lock().await;
                    state.pending_request = Some(PendingRequest {
                        request_id: id,
                        tool_call_id: tool_call_id.clone(),
                        kind: PendingRequestKind::UserInput {
                            questions: question_ids,
                        },
                    });
                }

                self.emit_event(json!({ "kind": "TypingStatusChanged", "data": false }));
                self.emit_event(json!({
                    "kind": "ToolRequest",
                    "data": {
                        "tool_call_id": tool_call_id,
                        "tool_name": "ask_user_question",
                        "tool_type": {
                            "kind": "Other",
                            "args": {
                                "questions": questions,
                                "type": "request_user_input"
                            }
                        }
                    }
                }));
            }
            "item/tool/call" => {
                let call_id = params
                    .get("callId")
                    .and_then(Value::as_str)
                    .unwrap_or("dynamic-tool-call");
                let tool_name = params
                    .get("tool")
                    .and_then(Value::as_str)
                    .unwrap_or("dynamic_tool");

                self.emit_event(json!({
                    "kind": "ToolRequest",
                    "data": {
                        "tool_call_id": call_id,
                        "tool_name": tool_name,
                        "tool_type": {
                            "kind": "Other",
                            "args": {
                                "type": "dynamic_tool_call",
                                "arguments": params.get("arguments").cloned().unwrap_or(Value::Null)
                            }
                        }
                    }
                }));

                let response_payload = json!({
                    "success": false,
                    "contentItems": [
                        {
                            "type": "inputText",
                            "text": "Dynamic client tool calls are not yet supported in Tyde."
                        }
                    ]
                });
                let _ = self.rpc.respond(id, response_payload).await;
                self.emit_tool_execution_completed(
                    call_id,
                    tool_name,
                    false,
                    json!({
                        "kind": "Error",
                        "short_message": "Dynamic client tool calls are not yet supported in Tyde.",
                        "detailed_message": "Codex requested a client-side dynamic tool call that Tyde has not implemented yet."
                    }),
                    Some("Dynamic client tool calls are not yet supported in Tyde.".to_string()),
                );
            }
            _ => {
                let _ = self
                    .rpc
                    .respond(
                        id,
                        json!({"ignored": true, "reason": "unsupported_server_request"}),
                    )
                    .await;
            }
        }
    }

    async fn add_active_turn_tool_bytes(&self, bytes: u64) {
        if bytes == 0 {
            return;
        }
        let mut state = self.state.lock().await;
        let Some(turn_id) = state.active_turn_id.as_ref().cloned() else {
            return;
        };
        let estimate = state.turn_context_by_turn.entry(turn_id).or_default();
        estimate.tool_io_bytes = estimate.tool_io_bytes.saturating_add(bytes);
    }

    async fn handle_item_started(&self, params: &Value) {
        let Some(item) = params.get("item") else {
            return;
        };
        let item_type = item.get("type").and_then(Value::as_str).unwrap_or_default();
        let item_id = item
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("tool-call")
            .to_string();

        match item_type {
            "commandExecution" => {
                let command = item
                    .get("command")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let cwd = item
                    .get("cwd")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                self.emit_event(json!({
                    "kind": "ToolRequest",
                    "data": {
                        "tool_call_id": item_id,
                        "tool_name": "run_command",
                        "tool_type": {
                            "kind": "RunCommand",
                            "command": command,
                            "working_directory": cwd
                        }
                    }
                }));
            }
            "fileChange" => {
                let file_changes = parse_codex_file_changes(item);
                if file_changes.is_empty() {
                    return;
                }

                let total = file_changes.len();
                let call_ids = file_changes
                    .iter()
                    .enumerate()
                    .map(|(idx, _)| codex_file_change_call_id(&item_id, idx, total))
                    .collect::<Vec<_>>();

                {
                    let mut state = self.state.lock().await;
                    state
                        .file_change_call_ids
                        .insert(item_id.clone(), call_ids.clone());
                }

                for (change, call_id) in file_changes.into_iter().zip(call_ids.into_iter()) {
                    self.emit_modify_file_request(
                        &call_id,
                        &change.path,
                        &change.before,
                        &change.after,
                    );
                }
            }
            "mcpToolCall" | "dynamicToolCall" => {
                let tool_name = item
                    .get("tool")
                    .and_then(Value::as_str)
                    .unwrap_or(item_type)
                    .to_string();
                self.emit_event(json!({
                    "kind": "ToolRequest",
                    "data": {
                        "tool_call_id": item_id,
                        "tool_name": tool_name,
                        "tool_type": {
                            "kind": "Other",
                            "args": item
                        }
                    }
                }));
            }
            _ => {}
        }
    }

    async fn handle_item_completed(&self, params: &Value) {
        let Some(item) = params.get("item") else {
            return;
        };

        let item_type = item.get("type").and_then(Value::as_str).unwrap_or_default();
        let item_id = item
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("item")
            .to_string();

        match item_type {
            "agentMessage" => {
                let text = item
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let (turn_id, reasoning, model, token_usage, turn_context) = {
                    let mut state = self.state.lock().await;
                    let stream = state
                        .active_stream
                        .take()
                        .unwrap_or_else(|| ActiveStreamState {
                            turn_id: state
                                .active_turn_id
                                .clone()
                                .unwrap_or_else(|| "turn".to_string()),
                            message_id: item_id.clone(),
                            text: text.clone(),
                            reasoning: String::new(),
                        });
                    let model = state.model.clone().unwrap_or_else(|| "codex".to_string());
                    let token = state.token_usage_by_turn.get(&stream.turn_id).cloned();
                    let turn_context = state
                        .turn_context_by_turn
                        .get(&stream.turn_id)
                        .cloned()
                        .unwrap_or_default();
                    state.conversation_bytes_total = state
                        .conversation_bytes_total
                        .saturating_add(text.len() as u64);
                    (stream.turn_id, stream.reasoning, model, token, turn_context)
                };
                let reasoning = if reasoning.trim().is_empty() {
                    extract_codex_item_reasoning(item).unwrap_or(reasoning)
                } else {
                    reasoning
                };
                let context_breakdown =
                    estimate_context_breakdown(token_usage.as_ref(), &turn_context);
                self.emit_event(json!({
                    "kind": "StreamEnd",
                    "data": {
                        "message": {
                            "timestamp": unix_now_ms(),
                            "sender": { "Assistant": { "agent": CODEX_AGENT_NAME } },
                            "content": text,
                            "reasoning": if reasoning.is_empty() {
                                Value::Null
                            } else {
                                json!({ "text": reasoning })
                            },
                            "tool_calls": [],
                            "model_info": { "model": model },
                            "token_usage": token_usage,
                            "context_breakdown": context_breakdown,
                            "images": []
                        }
                    }
                }));

                // If turn completion arrived before this message item, clean up now.
                // Otherwise keep usage/context until turn completion so follow-up
                // agentMessage items in the same turn can still read them.
                let mut state = self.state.lock().await;
                let turn_still_active = state.active_turn_id.as_deref() == Some(turn_id.as_str());
                if !turn_still_active {
                    state.token_usage_by_turn.remove(&turn_id);
                    state.turn_context_by_turn.remove(&turn_id);
                }
            }
            "userMessage" => {
                // User messages are emitted synchronously when sent to keep ordering stable.
            }
            "reasoning" => {
                let Some(reasoning_text) = extract_codex_item_reasoning(item) else {
                    return;
                };
                let reasoning_text = reasoning_text.trim().to_string();
                if reasoning_text.is_empty() {
                    return;
                }

                let mut should_emit = false;
                {
                    let mut state = self.state.lock().await;
                    if let Some(stream) = state.active_stream.as_mut() {
                        let duplicate = stream
                            .reasoning
                            .split('\n')
                            .any(|line| line.trim() == reasoning_text.as_str());
                        if !duplicate {
                            if !stream.reasoning.is_empty() && !stream.reasoning.ends_with('\n') {
                                stream.reasoning.push('\n');
                            }
                            stream.reasoning.push_str(&reasoning_text);
                            should_emit = true;
                        }
                    }
                    if should_emit {
                        if let Some(turn_id) = state.active_turn_id.as_ref().cloned() {
                            let estimate = state.turn_context_by_turn.entry(turn_id).or_default();
                            estimate.reasoning_bytes = estimate
                                .reasoning_bytes
                                .saturating_add(reasoning_text.len() as u64);
                        }
                    }
                }

                if should_emit {
                    self.emit_event(json!({
                        "kind": "StreamReasoningDelta",
                        "data": {
                            "message_id": item_id,
                            "text": reasoning_text
                        }
                    }));
                }
            }
            "commandExecution" => {
                self.add_active_turn_tool_bytes(estimate_command_execution_tool_bytes(item))
                    .await;
                let exit_code = item.get("exitCode").and_then(Value::as_i64).unwrap_or(-1) as i32;
                let output = item
                    .get("aggregatedOutput")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let success = exit_code == 0;
                self.emit_tool_execution_completed(
                    &item_id,
                    "run_command",
                    success,
                    json!({
                        "kind": "RunCommand",
                        "exit_code": exit_code,
                        "stdout": output,
                        "stderr": ""
                    }),
                    if success {
                        None
                    } else {
                        Some(format!("Command failed with exit code {exit_code}"))
                    },
                );
            }
            "fileChange" => {
                self.add_active_turn_tool_bytes(estimate_file_change_tool_bytes(item))
                    .await;
                let success = item.get("status").and_then(Value::as_str) == Some("completed");
                let known_call_ids = {
                    let mut state = self.state.lock().await;
                    state
                        .file_change_call_ids
                        .remove(&item_id)
                        .unwrap_or_default()
                };
                let file_changes = parse_codex_file_changes(item);

                if !file_changes.is_empty() {
                    let total = file_changes.len();
                    for (idx, change) in file_changes.iter().enumerate() {
                        let call_id = known_call_ids
                            .get(idx)
                            .cloned()
                            .unwrap_or_else(|| codex_file_change_call_id(&item_id, idx, total));

                        if known_call_ids.get(idx).is_none() {
                            self.emit_modify_file_request(
                                &call_id,
                                &change.path,
                                &change.before,
                                &change.after,
                            );
                        }

                        self.emit_tool_execution_completed(
                            &call_id,
                            "modify_file",
                            success,
                            json!({
                                "kind": "ModifyFile",
                                "lines_added": change.lines_added,
                                "lines_removed": change.lines_removed
                            }),
                            if success {
                                None
                            } else {
                                Some("File changes were not applied".to_string())
                            },
                        );
                    }

                    for call_id in known_call_ids.iter().skip(total) {
                        self.emit_tool_execution_completed(
                            call_id,
                            "modify_file",
                            success,
                            json!({
                                "kind": "ModifyFile",
                                "lines_added": 0,
                                "lines_removed": 0
                            }),
                            if success {
                                None
                            } else {
                                Some("File changes were not applied".to_string())
                            },
                        );
                    }
                    return;
                }

                if !known_call_ids.is_empty() {
                    for call_id in known_call_ids {
                        self.emit_tool_execution_completed(
                            &call_id,
                            "modify_file",
                            success,
                            json!({
                                "kind": "ModifyFile",
                                "lines_added": 0,
                                "lines_removed": 0
                            }),
                            if success {
                                None
                            } else {
                                Some("File changes were not applied".to_string())
                            },
                        );
                    }
                    return;
                }

                self.emit_tool_execution_completed(
                    &item_id,
                    "file_change",
                    success,
                    json!({
                        "kind": "Other",
                        "result": item
                    }),
                    if success {
                        None
                    } else {
                        Some("File changes were not applied".to_string())
                    },
                );
            }
            "mcpToolCall" | "dynamicToolCall" => {
                self.add_active_turn_tool_bytes(estimate_generic_tool_bytes(item))
                    .await;
                let tool_name = item
                    .get("tool")
                    .and_then(Value::as_str)
                    .unwrap_or(item_type);
                let success = item.get("status").and_then(Value::as_str) == Some("completed")
                    || item.get("success").and_then(Value::as_bool) == Some(true);
                self.emit_tool_execution_completed(
                    &item_id,
                    tool_name,
                    success,
                    json!({
                        "kind": "Other",
                        "result": item
                    }),
                    if success {
                        None
                    } else {
                        Some(format!("{tool_name} failed"))
                    },
                );
            }
            _ => {}
        }
    }

    fn handle_plan_update(&self, params: &Value) {
        let title = params
            .get("explanation")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .unwrap_or("Plan")
            .to_string();

        let tasks = params
            .get("plan")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .enumerate()
            .map(|(idx, step)| {
                let status = step
                    .get("status")
                    .and_then(Value::as_str)
                    .map(map_plan_status)
                    .unwrap_or("pending");
                json!({
                    "id": idx as u64 + 1,
                    "description": step.get("step").and_then(Value::as_str).unwrap_or("step"),
                    "status": status
                })
            })
            .collect::<Vec<_>>();

        self.emit_event(json!({
            "kind": "TaskUpdate",
            "data": {
                "title": title,
                "tasks": tasks
            }
        }));
    }

    async fn handle_turn_completed(&self, params: &Value) {
        let turn_status = params
            .get("turn")
            .and_then(|v| v.get("status"))
            .and_then(Value::as_str)
            .unwrap_or("completed")
            .to_string();
        let turn_usage = extract_turn_token_usage(params);

        {
            let mut state = self.state.lock().await;
            if let Some((turn_id, token_usage)) = turn_usage {
                state.token_usage_by_turn.insert(turn_id, token_usage);
            }

            let completed_turn_id =
                extract_turn_id(params).or_else(|| state.active_turn_id.clone());
            state.active_turn_id = None;
            if let Some(turn_id) = completed_turn_id {
                let stream_open_for_turn = state
                    .active_stream
                    .as_ref()
                    .map(|stream| stream.turn_id == turn_id)
                    .unwrap_or(false);
                if !stream_open_for_turn {
                    state.turn_context_by_turn.remove(&turn_id);
                    state.token_usage_by_turn.remove(&turn_id);
                }
            }
            state.pending_request = None;
            state.file_change_call_ids.clear();
            state.pending_user_input_bytes = 0;
        }

        self.emit_event(json!({ "kind": "TypingStatusChanged", "data": false }));

        if turn_status == "interrupted" {
            self.emit_event(json!({
                "kind": "OperationCancelled",
                "data": { "message": "Operation cancelled" }
            }));
            return;
        }

        if turn_status == "failed" {
            let message = params
                .get("turn")
                .and_then(|v| v.get("error"))
                .and_then(|v| v.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("Codex turn failed")
                .to_string();
            self.emit_event(json!({ "kind": "Error", "data": message }));
        }
    }

    fn emit_tool_execution_completed(
        &self,
        tool_call_id: &str,
        tool_name: &str,
        success: bool,
        tool_result: Value,
        error: Option<String>,
    ) {
        self.emit_event(json!({
            "kind": "ToolExecutionCompleted",
            "data": {
                "tool_call_id": tool_call_id,
                "tool_name": tool_name,
                "tool_result": tool_result,
                "success": success,
                "error": error
            }
        }));
    }

    fn emit_modify_file_request(
        &self,
        tool_call_id: &str,
        file_path: &str,
        before: &str,
        after: &str,
    ) {
        self.emit_event(json!({
            "kind": "ToolRequest",
            "data": {
                "tool_call_id": tool_call_id,
                "tool_name": "modify_file",
                "tool_type": {
                    "kind": "ModifyFile",
                    "file_path": file_path,
                    "before": before,
                    "after": after
                }
            }
        }));
    }

    fn emit_user_message_added(&self, content: &str, images: Option<&[ImageAttachment]>) {
        let image_payload = images
            .unwrap_or(&[])
            .iter()
            .map(|image| {
                json!({
                    "media_type": image.media_type,
                    "data": image.data
                })
            })
            .collect::<Vec<_>>();

        self.emit_event(json!({
            "kind": "MessageAdded",
            "data": {
                "timestamp": unix_now_ms(),
                "sender": "User",
                "content": content,
                "tool_calls": [],
                "images": image_payload
            }
        }));
    }

    fn emit_event(&self, event: Value) {
        if let Err(e) = self.event_tx.send(event) {
            tracing::trace!("event send failed: {e}");
        }
    }
}

fn codex_thread_to_session_metadata(thread: &Value) -> Option<Value> {
    let session_id = thread.get("id").and_then(Value::as_str)?;
    let preview = thread
        .get("preview")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let title = thread
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            if preview.trim().is_empty() {
                "Codex Session".to_string()
            } else {
                preview.clone()
            }
        });

    let created_at = thread
        .get("createdAt")
        .and_then(Value::as_u64)
        .unwrap_or_else(unix_now_ms);
    let last_modified = thread
        .get("updatedAt")
        .and_then(Value::as_u64)
        .unwrap_or(created_at);
    let workspace_root = thread
        .get("cwd")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    let message_count: Option<u64> = thread.get("turns").and_then(Value::as_array).map(|turns| {
        turns
            .iter()
            .filter_map(|turn| turn.get("items").and_then(Value::as_array))
            .map(|items| {
                items
                    .iter()
                    .filter(|item| {
                        matches!(
                            item.get("type").and_then(Value::as_str),
                            Some("userMessage" | "agentMessage")
                        )
                    })
                    .count() as u64
            })
            .sum::<u64>()
    });

    Some(json!({
        "id": session_id,
        "session_id": session_id,
        "title": title,
        "created_at": created_at,
        "last_modified": last_modified,
        "last_message_preview": preview,
        "workspace_root": workspace_root,
        "message_count": message_count,
        "backend_kind": "codex"
    }))
}

fn extract_codex_item_text(item: &Value) -> String {
    if let Some(text) = item.get("text").and_then(Value::as_str) {
        if !text.trim().is_empty() {
            return text.to_string();
        }
    }

    let mut chunks: Vec<String> = Vec::new();
    if let Some(content) = item.get("content").and_then(Value::as_array) {
        for part in content {
            if let Some(text) = part.get("text").and_then(Value::as_str) {
                if !text.trim().is_empty() {
                    chunks.push(text.to_string());
                }
                continue;
            }
            if let Some(text) = part.get("inputText").and_then(Value::as_str) {
                if !text.trim().is_empty() {
                    chunks.push(text.to_string());
                }
                continue;
            }
            if let Some(text) = part.get("value").and_then(Value::as_str) {
                if !text.trim().is_empty() {
                    chunks.push(text.to_string());
                }
            }
        }
    }

    if chunks.is_empty() {
        String::new()
    } else {
        chunks.join("\n")
    }
}

fn extract_codex_reasoning_delta_text(params: &Value) -> Option<String> {
    for key in [
        "delta",
        "text",
        "summaryText",
        "summary_text",
        "reasoningSummary",
        "reasoning_summary",
        "reasoningSummaryText",
        "reasoning_summary_text",
        "summary",
        "reasoning",
        "thinking",
        "content",
    ] {
        if let Some(text) = extract_codex_reasoning_delta_fragment(params.get(key)) {
            return Some(text);
        }
    }

    for nested in ["msg", "event", "payload"] {
        if let Some(value) = params.get(nested) {
            if let Some(text) = extract_codex_reasoning_delta_text(value) {
                return Some(text);
            }
        }
    }

    params.get("item").and_then(extract_codex_item_reasoning)
}

fn extract_codex_reasoning_delta_fragment(value: Option<&Value>) -> Option<String> {
    let value = value?;
    match value {
        Value::String(text) => {
            if text.is_empty() {
                None
            } else {
                Some(text.to_string())
            }
        }
        Value::Array(values) => {
            let mut out = String::new();
            for part in values {
                if let Some(text) = extract_codex_reasoning_delta_fragment(Some(part)) {
                    out.push_str(&text);
                }
            }
            if out.is_empty() {
                None
            } else {
                Some(out)
            }
        }
        Value::Object(map) => {
            for key in [
                "delta",
                "summary_delta",
                "summaryDelta",
                "reasoning_delta",
                "reasoningDelta",
                "text",
                "value",
                "token",
                "output_text",
                "outputText",
                "summaryText",
                "summary_text",
                "summary",
                "reasoningSummary",
                "reasoning_summary",
                "reasoningSummaryText",
                "reasoning_summary_text",
                "reasoning",
                "thinking",
                "content",
                "parts",
            ] {
                if let Some(text) = extract_codex_reasoning_delta_fragment(map.get(key)) {
                    return Some(text);
                }
            }
            None
        }
        _ => None,
    }
}

fn extract_reasoning_delta_from_legacy_codex_event(method: &str, params: &Value) -> Option<String> {
    let event_type = extract_codex_event_type(method, params)?;
    if event_type == "agent_reasoning_section_break" {
        return Some("\n\n".to_string());
    }
    if !is_codex_event_reasoning_type(&event_type) {
        return None;
    }
    extract_codex_reasoning_delta_text(params)
}

fn apply_reasoning_delta_to_state(
    state: &mut CodexState,
    params: &Value,
    delta: &str,
) -> Option<Value> {
    if delta.is_empty() {
        return None;
    }

    let delta = if let Some(stream) = state.active_stream.as_ref() {
        merge_reasoning_delta(&stream.reasoning, delta)
    } else {
        delta.to_string()
    };
    if delta.is_empty() {
        return None;
    }

    let message_id = params
        .get("itemId")
        .and_then(Value::as_str)
        .or_else(|| params.get("item_id").and_then(Value::as_str))
        .or_else(|| params.get("id").and_then(Value::as_str))
        .map(|id| id.to_string())
        .or_else(|| {
            state
                .active_stream
                .as_ref()
                .map(|stream| stream.message_id.clone())
                .filter(|id| !id.is_empty())
        })
        .unwrap_or_else(|| "assistant".to_string());

    if let Some(stream) = state.active_stream.as_mut() {
        stream.message_id = message_id.clone();
        stream.reasoning.push_str(&delta);
    }
    if let Some(turn_id) = state.active_turn_id.as_ref().cloned() {
        let estimate = state.turn_context_by_turn.entry(turn_id).or_default();
        estimate.reasoning_bytes = estimate.reasoning_bytes.saturating_add(delta.len() as u64);
    }

    Some(json!({
        "kind": "StreamReasoningDelta",
        "data": {
            "message_id": message_id,
            "text": delta
        }
    }))
}

fn merge_reasoning_delta(existing: &str, incoming: &str) -> String {
    if incoming.is_empty() {
        return String::new();
    }
    if existing.is_empty() {
        return incoming.to_string();
    }
    if incoming.len() >= 8 && existing.contains(incoming) {
        return String::new();
    }
    if incoming == existing {
        return String::new();
    }
    if let Some(suffix) = incoming.strip_prefix(existing) {
        return suffix.to_string();
    }
    if existing.ends_with(incoming) {
        return String::new();
    }

    let overlap = longest_suffix_prefix_overlap(existing, incoming);
    if overlap > 0 {
        incoming[overlap..].to_string()
    } else {
        incoming.to_string()
    }
}

fn longest_suffix_prefix_overlap(existing: &str, incoming: &str) -> usize {
    let mut boundaries = incoming
        .char_indices()
        .map(|(idx, _)| idx)
        .collect::<Vec<_>>();
    boundaries.push(incoming.len());

    for len in boundaries.into_iter().rev() {
        if len == 0 || len > existing.len() {
            continue;
        }
        let start = existing.len() - len;
        if !existing.is_char_boundary(start) {
            continue;
        }
        if existing[start..] == incoming[..len] {
            return len;
        }
    }
    0
}

fn extract_codex_event_type(method: &str, params: &Value) -> Option<String> {
    params
        .get("type")
        .and_then(Value::as_str)
        .or_else(|| {
            params
                .get("msg")
                .and_then(|msg| msg.get("type"))
                .and_then(Value::as_str)
        })
        .or_else(|| method.strip_prefix("codex/event/"))
        .map(|raw| raw.trim().to_ascii_lowercase())
}

fn is_codex_event_reasoning_type(event_type: &str) -> bool {
    matches!(
        event_type,
        "agent_reasoning"
            | "agent_reasoning_delta"
            | "agent_reasoning_raw_content"
            | "agent_reasoning_raw_content_delta"
    )
}

fn extract_codex_item_reasoning(item: &Value) -> Option<String> {
    extract_codex_reasoning_fragment(item.get("reasoning"))
        .or_else(|| extract_codex_reasoning_fragment(item.get("summaryText")))
        .or_else(|| extract_codex_reasoning_fragment(item.get("summary")))
        .or_else(|| extract_codex_reasoning_fragment(item.get("reasoningSummary")))
        .or_else(|| {
            let mut chunks = Vec::new();
            if let Some(content) = item.get("content").and_then(Value::as_array) {
                for part in content {
                    let part_type = part
                        .get("type")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_ascii_lowercase();
                    if !part_type.contains("reason")
                        && !part_type.contains("think")
                        && !part_type.contains("summary")
                    {
                        continue;
                    }
                    if let Some(text) = extract_codex_reasoning_fragment(Some(part)) {
                        chunks.push(text);
                    }
                }
            }
            join_nonempty_chunks(chunks)
        })
}

fn extract_codex_reasoning_fragment(value: Option<&Value>) -> Option<String> {
    let value = value?;
    match value {
        Value::String(text) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        }
        Value::Array(values) => {
            let mut chunks = Vec::new();
            for part in values {
                if let Some(text) = extract_codex_reasoning_fragment(Some(part)) {
                    chunks.push(text);
                }
            }
            join_nonempty_chunks(chunks)
        }
        Value::Object(map) => {
            for key in [
                "text",
                "summaryText",
                "summary_text",
                "summary",
                "reasoningSummary",
                "reasoning_summary",
                "reasoningSummaryText",
                "reasoning_summary_text",
                "reasoning",
                "thinking",
                "output_text",
                "outputText",
                "delta",
                "summary_delta",
                "summaryDelta",
                "reasoning_delta",
                "reasoningDelta",
                "token",
                "value",
                "content",
                "parts",
            ] {
                if let Some(text) = extract_codex_reasoning_fragment(map.get(key)) {
                    return Some(text);
                }
            }
            None
        }
        _ => None,
    }
}

fn is_reasoning_notification_method(method: &str) -> bool {
    let normalized = method.to_ascii_lowercase();
    normalized.starts_with("item/reasoning/")
        || normalized.starts_with("item/reasoning")
        || normalized.starts_with("item/thinking/")
        || normalized.starts_with("item/thinking")
}

fn is_terminal_codex_error_notification(state: &CodexState, params: &Value) -> bool {
    if params.get("fatal").and_then(Value::as_bool) == Some(true)
        || params.get("terminal").and_then(Value::as_bool) == Some(true)
        || params.get("recoverable").and_then(Value::as_bool) == Some(false)
    {
        return true;
    }

    state.active_turn_id.is_none()
        && state.active_stream.is_none()
        && state.pending_request.is_none()
}

fn join_nonempty_chunks(chunks: Vec<String>) -> Option<String> {
    let normalized = chunks
        .into_iter()
        .map(|chunk| chunk.trim().to_string())
        .filter(|chunk| !chunk.is_empty())
        .collect::<Vec<_>>();
    if normalized.is_empty() {
        None
    } else {
        Some(normalized.join("\n"))
    }
}

fn map_plan_status(status: &str) -> &'static str {
    match status {
        "completed" => "completed",
        "inProgress" => "in_progress",
        "pending" => "pending",
        _ => "pending",
    }
}

#[derive(Debug, Clone)]
struct CodexFileChange {
    path: String,
    before: String,
    after: String,
    lines_added: u64,
    lines_removed: u64,
}

fn codex_file_change_call_id(item_id: &str, index: usize, total: usize) -> String {
    if total <= 1 {
        item_id.to_string()
    } else {
        format!("{item_id}#{}", index + 1)
    }
}

fn parse_codex_file_changes(item: &Value) -> Vec<CodexFileChange> {
    let Some(changes) = item.get("changes").and_then(Value::as_array) else {
        return Vec::new();
    };

    let mut parsed = Vec::new();
    for change in changes {
        let path = change
            .get("path")
            .and_then(Value::as_str)
            .filter(|v| !v.trim().is_empty())
            .or_else(|| {
                change
                    .get("kind")
                    .and_then(|k| k.get("move_path"))
                    .and_then(Value::as_str)
            })
            .unwrap_or_default()
            .to_string();
        if path.trim().is_empty() {
            continue;
        }

        let diff = change
            .get("diff")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let (before, after, lines_added, lines_removed) = parse_unified_diff_preview(diff);

        parsed.push(CodexFileChange {
            path,
            before,
            after,
            lines_added,
            lines_removed,
        });
    }

    parsed
}

fn parse_unified_diff_preview(diff: &str) -> (String, String, u64, u64) {
    let mut before_lines: Vec<String> = Vec::new();
    let mut after_lines: Vec<String> = Vec::new();
    let mut lines_added = 0u64;
    let mut lines_removed = 0u64;

    for line in diff.lines() {
        if line.starts_with("@@") || line.starts_with('\\') || line.is_empty() {
            continue;
        }

        if let Some(text) = line.strip_prefix('+') {
            // Skip patch file headers (`+++`) while counting actual additions.
            if !line.starts_with("+++ ") {
                after_lines.push(text.to_string());
                lines_added += 1;
            }
            continue;
        }

        if let Some(text) = line.strip_prefix('-') {
            // Skip patch file headers (`---`) while counting actual removals.
            if !line.starts_with("--- ") {
                before_lines.push(text.to_string());
                lines_removed += 1;
            }
            continue;
        }

        if let Some(text) = line.strip_prefix(' ') {
            before_lines.push(text.to_string());
            after_lines.push(text.to_string());
            continue;
        }

        before_lines.push(line.to_string());
        after_lines.push(line.to_string());
    }

    (
        before_lines.join("\n"),
        after_lines.join("\n"),
        lines_added,
        lines_removed,
    )
}

fn usage_u64(value: &Value, keys: &[&str]) -> Option<u64> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_u64))
}

fn extract_turn_id(params: &Value) -> Option<String> {
    params
        .get("turnId")
        .and_then(Value::as_str)
        .or_else(|| params.get("turn_id").and_then(Value::as_str))
        .or_else(|| params.get("id").and_then(Value::as_str))
        .or_else(|| {
            params
                .get("turn")
                .and_then(|turn| turn.get("id"))
                .and_then(Value::as_str)
        })
        .or_else(|| {
            params
                .get("turn")
                .and_then(|turn| turn.get("turnId"))
                .and_then(Value::as_str)
        })
        .or_else(|| {
            params
                .get("turn")
                .and_then(|turn| turn.get("turn_id"))
                .and_then(Value::as_str)
        })
        .map(|id| id.to_string())
}

fn extract_turn_token_usage_value(params: &Value) -> Option<&Value> {
    params
        .get("tokenUsage")
        .or_else(|| params.get("token_usage"))
        .or_else(|| params.get("usage"))
        .or_else(|| params.get("turn").and_then(|turn| turn.get("tokenUsage")))
        .or_else(|| params.get("turn").and_then(|turn| turn.get("token_usage")))
        .or_else(|| params.get("turn").and_then(|turn| turn.get("usage")))
}

fn extract_turn_token_usage(params: &Value) -> Option<(String, Value)> {
    let turn_id = extract_turn_id(params)?;
    let usage = extract_turn_token_usage_value(params)?;
    Some((turn_id, normalize_token_usage(usage)))
}

fn normalize_token_usage(raw: &Value) -> Value {
    let source = raw
        .get("last")
        .filter(|value| value.is_object())
        .unwrap_or(raw);

    // OpenAI convention: `inputTokens` is the TOTAL including cached tokens,
    // and `cachedInputTokens` is a subset.  Our internal contract (matching
    // Anthropic) expects `input_tokens` to be the non-cached portion only,
    // with cache fields as separate additive values.
    let cached_prompt_tokens =
        usage_u64(source, &["cachedInputTokens", "cached_prompt_tokens"]).unwrap_or(0);
    let cache_creation_input_tokens = usage_u64(
        source,
        &["cacheCreationInputTokens", "cache_creation_input_tokens"],
    )
    .unwrap_or(0);
    let raw_input_tokens = usage_u64(source, &["inputTokens"]).unwrap_or(0);
    let input_tokens = if source.get("inputTokens").is_some() {
        raw_input_tokens
            .saturating_sub(cached_prompt_tokens)
            .saturating_sub(cache_creation_input_tokens)
    } else {
        usage_u64(source, &["input_tokens", "inputTokens", "prompt_tokens"]).unwrap_or(0)
    };
    let prompt_tokens_total = if raw_input_tokens > 0 {
        raw_input_tokens
    } else {
        input_tokens
            .saturating_add(cached_prompt_tokens)
            .saturating_add(cache_creation_input_tokens)
    };

    // OpenAI convention: `outputTokens` includes reasoning.  Our contract
    // treats `reasoning_tokens` as an informational subset of `output_tokens`,
    // so `output_tokens` is stored as-is (already includes reasoning).
    let output_tokens = usage_u64(
        source,
        &["outputTokens", "output_tokens", "completion_tokens"],
    )
    .unwrap_or(0);
    let reasoning_tokens =
        usage_u64(source, &["reasoningOutputTokens", "reasoning_tokens"]).unwrap_or(0);

    // total_tokens = input_tokens + output_tokens (no double-counting).
    let total_tokens =
        usage_u64(source, &["totalTokens", "total_tokens"]).unwrap_or(input_tokens + output_tokens);
    let context_window = context_window_from_token_usage(raw, source)
        .filter(|window| *window > 0)
        .unwrap_or_else(|| {
            std::cmp::max(CODEX_ESTIMATED_CONTEXT_WINDOW, prompt_tokens_total.max(1))
        });

    json!({
        "input_tokens": input_tokens,
        "output_tokens": output_tokens,
        "total_tokens": total_tokens,
        "cached_prompt_tokens": cached_prompt_tokens,
        "cache_creation_input_tokens": cache_creation_input_tokens,
        "reasoning_tokens": reasoning_tokens,
        "context_window": context_window
    })
}

fn context_window_from_token_usage(raw: &Value, last: &Value) -> Option<u64> {
    const WINDOW_KEYS: &[&str] = &[
        "contextWindow",
        "context_window",
        "maxInputTokens",
        "max_input_tokens",
        "maxTokens",
        "max_tokens",
        "maxPromptTokens",
        "max_prompt_tokens",
    ];

    for key in WINDOW_KEYS {
        if let Some(value) = raw.get(*key).and_then(Value::as_u64) {
            return Some(value);
        }
    }

    for key in WINDOW_KEYS {
        if let Some(value) = last.get(*key).and_then(Value::as_u64) {
            return Some(value);
        }
    }

    None
}

fn estimate_context_breakdown(
    token_usage: Option<&Value>,
    turn_context: &TurnContextEstimate,
) -> Value {
    let base_input_tokens = token_usage
        .and_then(|usage| usage.get("input_tokens").and_then(Value::as_u64))
        .unwrap_or(0);
    let cached_prompt_tokens = token_usage
        .and_then(|usage| usage.get("cached_prompt_tokens").and_then(Value::as_u64))
        .unwrap_or(0);
    let cache_creation_input_tokens = token_usage
        .and_then(|usage| {
            usage
                .get("cache_creation_input_tokens")
                .and_then(Value::as_u64)
        })
        .unwrap_or(0);
    // Context utilization should reflect the full prompt footprint, including cache hits/writes.
    let mut input_tokens = base_input_tokens
        .saturating_add(cached_prompt_tokens)
        .saturating_add(cache_creation_input_tokens);
    let context_window = token_usage
        .and_then(|usage| usage.get("context_window").and_then(Value::as_u64))
        .filter(|window| *window > 0)
        .unwrap_or_else(|| std::cmp::max(CODEX_ESTIMATED_CONTEXT_WINDOW, input_tokens.max(1)));

    let reasoning_from_tokens = token_usage
        .and_then(|usage| usage.get("reasoning_tokens").and_then(Value::as_u64))
        .unwrap_or(0)
        .saturating_mul(CODEX_ESTIMATED_BYTES_PER_TOKEN);

    let reasoning_est = std::cmp::max(turn_context.reasoning_bytes, reasoning_from_tokens);
    let tools_est = turn_context.tool_io_bytes;
    let history_est = turn_context.conversation_history_bytes;
    let observed_bytes = reasoning_est
        .saturating_add(tools_est)
        .saturating_add(history_est);

    let mut total_prompt_bytes = input_tokens.saturating_mul(CODEX_ESTIMATED_BYTES_PER_TOKEN);
    if total_prompt_bytes == 0 {
        let system_floor = if observed_bytes > 0 {
            CODEX_MIN_SYSTEM_PROMPT_BYTES
        } else {
            0
        };
        total_prompt_bytes = observed_bytes.saturating_add(system_floor);
        if total_prompt_bytes > 0 {
            input_tokens = total_prompt_bytes.div_ceil(CODEX_ESTIMATED_BYTES_PER_TOKEN);
        }
    }

    let mut system_prompt_bytes = if total_prompt_bytes == 0 {
        0
    } else {
        let target = total_prompt_bytes / 10;
        std::cmp::max(CODEX_MIN_SYSTEM_PROMPT_BYTES, target)
    };
    system_prompt_bytes = std::cmp::min(system_prompt_bytes, total_prompt_bytes);

    let mut remaining = total_prompt_bytes.saturating_sub(system_prompt_bytes);
    let reasoning_bytes = std::cmp::min(reasoning_est, remaining);
    remaining = remaining.saturating_sub(reasoning_bytes);

    let tool_io_bytes = std::cmp::min(tools_est, remaining);
    remaining = remaining.saturating_sub(tool_io_bytes);

    let conversation_history_bytes = std::cmp::min(history_est, remaining);
    remaining = remaining.saturating_sub(conversation_history_bytes);

    let context_injection_bytes = remaining;

    json!({
        "system_prompt_bytes": system_prompt_bytes,
        "tool_io_bytes": tool_io_bytes,
        "conversation_history_bytes": conversation_history_bytes,
        "reasoning_bytes": reasoning_bytes,
        "context_injection_bytes": context_injection_bytes,
        "input_tokens": input_tokens,
        "context_window": context_window
    })
}

fn estimate_command_execution_tool_bytes(item: &Value) -> u64 {
    value_str_len(item, "command")
        .saturating_add(value_str_len(item, "cwd"))
        .saturating_add(value_str_len(item, "aggregatedOutput"))
}

fn estimate_file_change_tool_bytes(item: &Value) -> u64 {
    let mut total = 0u64;
    if let Some(changes) = item.get("changes").and_then(Value::as_array) {
        for change in changes {
            total = total
                .saturating_add(value_str_len(change, "path"))
                .saturating_add(value_str_len(change, "diff"));
        }
    }
    if total > 0 {
        return total;
    }
    estimate_generic_tool_bytes(item)
}

fn estimate_generic_tool_bytes(item: &Value) -> u64 {
    let bytes = serde_json::to_vec(item)
        .map(|v| v.len() as u64)
        .unwrap_or(0);
    std::cmp::min(bytes, 128_000)
}

fn value_str_len(value: &Value, key: &str) -> u64 {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(|v| v.len() as u64)
        .unwrap_or(0)
}

fn parse_approval_decision(message: &str) -> &'static str {
    let normalized = message.trim().to_ascii_lowercase();
    if normalized.starts_with("cancel") {
        return "cancel";
    }
    if normalized.contains("decline")
        || normalized.contains("deny")
        || normalized == "no"
        || normalized == "n"
    {
        return "decline";
    }
    if normalized.contains("always") || normalized.contains("for session") {
        return "acceptForSession";
    }
    "accept"
}

fn parse_review_decision(message: &str) -> &'static str {
    match parse_approval_decision(message) {
        "accept" => "approved",
        "acceptForSession" => "approved_for_session",
        "decline" => "denied",
        "cancel" => "abort",
        _ => "approved",
    }
}

fn normalize_reasoning_effort(raw: &str) -> Option<String> {
    let normalized = raw.trim().to_ascii_lowercase();
    let value = match normalized.as_str() {
        "off" | "none" => "none",
        "minimal" | "min" => "minimal",
        "low" => "low",
        "medium" | "med" => "medium",
        "high" => "high",
        "max" | "xhigh" => "xhigh",
        _ => return None,
    };
    Some(value.to_string())
}

fn pick_workspace_root(workspace_roots: &[String]) -> Result<String, String> {
    workspace_roots
        .iter()
        .find(|root| !root.trim().is_empty() && !root.starts_with("ssh://"))
        .cloned()
        .ok_or("Codex backend requires at least one local workspace root".to_string())
}

async fn persist_temp_image(image: &ImageAttachment) -> Result<String, String> {
    static IMAGE_COUNTER: AtomicU64 = AtomicU64::new(1);

    let bytes = BASE64_STANDARD
        .decode(image.data.trim())
        .map_err(|e| format!("Failed to decode image attachment '{}': {e}", image.name))?;

    let ext = media_type_to_extension(&image.media_type);
    let id = IMAGE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let ts_ms = unix_now_ms();

    let dir = std::env::temp_dir().join("tyde-codex-images");
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| format!("Failed to create temp image directory: {e}"))?;

    let file_name = format!("{}_{}_{}.{}", sanitize_name(&image.name), ts_ms, id, ext);
    let path = dir.join(file_name);
    tokio::fs::write(&path, bytes)
        .await
        .map_err(|e| format!("Failed to write temp image file: {e}"))?;

    Ok(path.to_string_lossy().to_string())
}

fn sanitize_name(name: &str) -> String {
    let cleaned = name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    if cleaned.is_empty() {
        "image".to_string()
    } else {
        cleaned
    }
}

fn media_type_to_extension(media_type: &str) -> &'static str {
    match media_type {
        "image/jpeg" | "image/jpg" => "jpg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/svg+xml" => "svg",
        "image/bmp" => "bmp",
        "image/tiff" => "tiff",
        _ => "png",
    }
}

fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0))
        .as_millis() as u64
}

#[derive(Clone)]
enum CodexInbound {
    Notification {
        method: String,
        params: Value,
    },
    ServerRequest {
        id: Value,
        method: String,
        params: Value,
    },
    Stderr(String),
    Closed {
        exit_code: Option<i32>,
    },
}

fn toml_quoted(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| format!("\"{value}\""))
}

fn codex_mcp_config_overrides(startup_mcp_servers: &[StartupMcpServer]) -> Vec<String> {
    let mut overrides = Vec::new();

    for server in startup_mcp_servers {
        let name = server.name.trim();
        if name.is_empty() {
            continue;
        }
        let base = format!("mcp_servers.{name}");
        match &server.transport {
            StartupMcpTransport::Http {
                url,
                headers,
                bearer_token_env_var,
            } => {
                let trimmed_url = url.trim();
                if trimmed_url.is_empty() {
                    continue;
                }
                overrides.push(format!("{base}.url={}", toml_quoted(trimmed_url)));
                if let Some(env_var) = bearer_token_env_var
                    .as_ref()
                    .map(|raw| raw.trim())
                    .filter(|raw| !raw.is_empty())
                {
                    overrides.push(format!(
                        "{base}.bearer_token_env_var={}",
                        toml_quoted(env_var)
                    ));
                }
                for (key, value) in headers {
                    let key = key.trim();
                    if key.is_empty() {
                        continue;
                    }
                    overrides.push(format!("{base}.http_headers.{key}={}", toml_quoted(value)));
                }
            }
            StartupMcpTransport::Stdio { command, args, env } => {
                let trimmed_command = command.trim();
                if trimmed_command.is_empty() {
                    continue;
                }
                overrides.push(format!("{base}.command={}", toml_quoted(trimmed_command)));
                if !args.is_empty() {
                    let args_literal =
                        serde_json::to_string(args).unwrap_or_else(|_| "[]".to_string());
                    overrides.push(format!("{base}.args={args_literal}"));
                }
                for (key, value) in env {
                    let key = key.trim();
                    if key.is_empty() {
                        continue;
                    }
                    overrides.push(format!("{base}.env.{key}={}", toml_quoted(value)));
                }
            }
        }
    }

    overrides
}

type PendingRpcMap = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

struct CodexRpc {
    stdin: Arc<Mutex<ChildStdin>>,
    pending: PendingRpcMap,
    next_id: AtomicU64,
    child: Arc<Mutex<Option<Child>>>,
}

impl CodexRpc {
    fn spawn(
        ssh_host: Option<&str>,
        startup_mcp_servers: &[StartupMcpServer],
    ) -> Result<(Self, mpsc::UnboundedReceiver<CodexInbound>), String> {
        let config_overrides = codex_mcp_config_overrides(startup_mcp_servers);
        let mut child = if let Some(host) = ssh_host {
            use crate::remote::shell_quote_command;

            let mut remote_args = vec![
                "codex".to_string(),
                "app-server".to_string(),
                "--listen".to_string(),
                "stdio://".to_string(),
            ];
            for override_key_value in &config_overrides {
                remote_args.push("-c".to_string());
                remote_args.push(override_key_value.clone());
            }
            let remote_cmd = format!(
                "PATH=\"$HOME/.cargo/bin:$HOME/.local/bin:/usr/local/bin:$PATH\" {}",
                shell_quote_command(&remote_args),
            );
            let mut cmd = Command::new("ssh");
            for arg in crate::remote::ssh_control_args()? {
                cmd.arg(arg);
            }
            cmd.arg("-T")
                .arg(host)
                .arg(remote_cmd)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .map_err(|e| format!("Failed to spawn Codex app-server over SSH: {e}"))?
        } else {
            let mut cmd = Command::new("codex");
            cmd.arg("app-server").arg("--listen").arg("stdio://");
            for override_key_value in &config_overrides {
                cmd.arg("-c").arg(override_key_value);
            }
            cmd.stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .map_err(|e| format!("Failed to spawn Codex app-server: {e}"))?
        };

        let stdin = child.stdin.take().ok_or("Failed to capture Codex stdin")?;
        let stdout = child
            .stdout
            .take()
            .ok_or("Failed to capture Codex stdout")?;
        let stderr = child
            .stderr
            .take()
            .ok_or("Failed to capture Codex stderr")?;

        let child_ref = Arc::new(Mutex::new(Some(child)));
        let pending: PendingRpcMap = Arc::new(Mutex::new(HashMap::new()));
        let (inbound_tx, inbound_rx) = mpsc::unbounded_channel();

        let stdout_pending = Arc::clone(&pending);
        let stdout_inbound = inbound_tx.clone();
        let stdout_child = Arc::clone(&child_ref);
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let parsed = match serde_json::from_str::<Value>(&line) {
                    Ok(v) => v,
                    Err(err) => {
                        tracing::warn!("Failed to parse Codex stdout JSON: {err}; line: {line}");
                        continue;
                    }
                };

                if let Some(id) = parsed.get("id").and_then(Value::as_u64) {
                    let has_method = parsed.get("method").is_some();
                    let has_result_or_error =
                        parsed.get("result").is_some() || parsed.get("error").is_some();
                    if has_result_or_error && !has_method {
                        let response = if let Some(result) = parsed.get("result") {
                            Ok(result.clone())
                        } else {
                            let err_obj = parsed.get("error").cloned().unwrap_or(Value::Null);
                            let msg = err_obj
                                .get("message")
                                .and_then(Value::as_str)
                                .map(|s| s.to_string())
                                .unwrap_or_else(|| format!("Codex JSON-RPC error: {err_obj}"));
                            Err(msg)
                        };
                        if let Some(tx) = stdout_pending.lock().await.remove(&id) {
                            let _ = tx.send(response);
                        }
                        continue;
                    }
                }

                if let Some(method) = parsed.get("method").and_then(Value::as_str) {
                    let params = parsed.get("params").cloned().unwrap_or(Value::Null);
                    if let Some(id) = parsed.get("id").cloned() {
                        let _ = stdout_inbound.send(CodexInbound::ServerRequest {
                            id,
                            method: method.to_string(),
                            params,
                        });
                    } else {
                        let _ = stdout_inbound.send(CodexInbound::Notification {
                            method: method.to_string(),
                            params,
                        });
                    }
                }
            }

            let exit_code = match stdout_child.lock().await.as_mut() {
                Some(child) => child
                    .try_wait()
                    .ok()
                    .flatten()
                    .and_then(|status| status.code()),
                None => None,
            };

            let mut pending = stdout_pending.lock().await;
            for (_, tx) in pending.drain() {
                let _ = tx.send(Err("Codex app-server exited before response".to_string()));
            }
            drop(pending);

            let _ = stdout_inbound.send(CodexInbound::Closed { exit_code });
        });

        let stderr_inbound = inbound_tx.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let _ = stderr_inbound.send(CodexInbound::Stderr(line));
            }
        });

        Ok((
            Self {
                stdin: Arc::new(Mutex::new(stdin)),
                pending,
                next_id: AtomicU64::new(1),
                child: child_ref,
            },
            inbound_rx,
        ))
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let payload = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        });

        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        if let Err(err) = self.send_json(&payload).await {
            let _ = self.pending.lock().await.remove(&id);
            return Err(err);
        }

        match tokio::time::timeout(CODEX_REQUEST_TIMEOUT, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err("Codex response channel closed".to_string()),
            Err(_) => {
                let _ = self.pending.lock().await.remove(&id);
                Err(format!("Codex request timed out for method '{method}'"))
            }
        }
    }

    async fn respond(&self, id: Value, result: Value) -> Result<(), String> {
        self.send_json(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result
        }))
        .await
    }

    async fn send_json(&self, value: &Value) -> Result<(), String> {
        let mut stdin = self.stdin.lock().await;
        let line = format!("{value}\n");
        stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| format!("Failed to write to Codex stdin: {e}"))
    }

    async fn shutdown(&self) {
        let mut child_guard = self.child.lock().await;
        let Some(mut child) = child_guard.take() else {
            return;
        };

        match tokio::time::timeout(CODEX_SHUTDOWN_TIMEOUT, child.wait()).await {
            Ok(_) => {}
            Err(_) => {
                let _ = child.kill().await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_codex_state() -> CodexState {
        CodexState {
            thread_id: "thread-test".to_string(),
            model: Some("codex".to_string()),
            reasoning_effort: Some("xhigh".to_string()),
            approval_policy: None,
            active_turn_id: Some("turn-test".to_string()),
            active_stream: Some(ActiveStreamState {
                turn_id: "turn-test".to_string(),
                message_id: "msg-seed".to_string(),
                text: String::new(),
                reasoning: String::new(),
            }),
            token_usage_by_turn: HashMap::new(),
            turn_context_by_turn: HashMap::new(),
            file_change_call_ids: HashMap::new(),
            pending_request: None,
            pending_user_input_bytes: 0,
            conversation_bytes_total: 0,
        }
    }

    #[test]
    fn apply_reasoning_delta_to_state_emits_event_and_updates_state() {
        let mut state = test_codex_state();
        let params = json!({ "itemId": "reason-item-1" });

        let event = apply_reasoning_delta_to_state(&mut state, &params, "Inspecting constraints.")
            .expect("reasoning event");

        assert_eq!(event["kind"], json!("StreamReasoningDelta"));
        assert_eq!(event["data"]["message_id"], json!("reason-item-1"));
        assert_eq!(event["data"]["text"], json!("Inspecting constraints."));

        let stream = state.active_stream.as_ref().expect("active stream");
        assert_eq!(stream.message_id, "reason-item-1");
        assert_eq!(stream.reasoning, "Inspecting constraints.");

        let ctx = state
            .turn_context_by_turn
            .get("turn-test")
            .expect("turn context");
        assert_eq!(
            ctx.reasoning_bytes,
            "Inspecting constraints.".as_bytes().len() as u64
        );
    }

    #[test]
    fn apply_reasoning_delta_to_state_falls_back_to_assistant_message_id() {
        let mut state = test_codex_state();
        state.active_stream = None;
        state.active_turn_id = None;
        let params = json!({});

        let event = apply_reasoning_delta_to_state(&mut state, &params, "No item id present.")
            .expect("reasoning event");

        assert_eq!(event["data"]["message_id"], json!("assistant"));
    }

    #[test]
    fn apply_reasoning_delta_to_state_ignores_duplicate_delta_payloads() {
        let mut state = test_codex_state();
        let params = json!({ "itemId": "reason-item-1" });

        let first =
            apply_reasoning_delta_to_state(&mut state, &params, "Planning targeted web search")
                .expect("first reasoning event");
        assert_eq!(first["data"]["text"], json!("Planning targeted web search"));

        let second =
            apply_reasoning_delta_to_state(&mut state, &params, "Planning targeted web search");
        assert!(
            second.is_none(),
            "duplicate deltas should not emit a second event"
        );

        let stream = state.active_stream.as_ref().expect("active stream");
        assert_eq!(stream.reasoning, "Planning targeted web search");
    }

    #[test]
    fn apply_reasoning_delta_to_state_appends_only_new_suffix_for_snapshot_payloads() {
        let mut state = test_codex_state();
        let params = json!({ "itemId": "reason-item-1" });

        let _ = apply_reasoning_delta_to_state(&mut state, &params, "Planning targeted")
            .expect("first reasoning event");
        let second =
            apply_reasoning_delta_to_state(&mut state, &params, "Planning targeted web search")
                .expect("second reasoning event");

        assert_eq!(second["data"]["text"], json!(" web search"));

        let stream = state.active_stream.as_ref().expect("active stream");
        assert_eq!(stream.reasoning, "Planning targeted web search");
    }

    #[test]
    fn extract_codex_reasoning_delta_text_preserves_leading_whitespace() {
        let payload = json!({ "delta": " targeted web search" });
        assert_eq!(
            extract_codex_reasoning_delta_text(&payload),
            Some(" targeted web search".to_string())
        );
    }

    #[test]
    fn extract_codex_reasoning_delta_text_parses_nested_shapes() {
        let payload = json!({
            "itemId": "abc",
            "delta": {
                "summary": {
                    "text": "Need to inspect parser edge-cases."
                }
            }
        });

        assert_eq!(
            extract_codex_reasoning_delta_text(&payload),
            Some("Need to inspect parser edge-cases.".to_string())
        );
    }

    #[test]
    fn merge_reasoning_delta_handles_overlap_and_duplicates() {
        assert_eq!(merge_reasoning_delta("", "Plan"), "Plan");
        assert_eq!(merge_reasoning_delta("Plan", "Plan"), "");
        assert_eq!(merge_reasoning_delta("Plan", "Plan more"), " more");
        assert_eq!(merge_reasoning_delta("Planning", "ing details"), " details");
        assert_eq!(merge_reasoning_delta("Planning details", "details"), "");
        assert_eq!(
            merge_reasoning_delta("Planning targeted web search", "targeted"),
            ""
        );
    }

    #[test]
    fn extract_codex_item_reasoning_reads_reasoning_content_blocks() {
        let item = json!({
            "type": "agentMessage",
            "content": [
                { "type": "text", "text": "Visible answer" },
                { "type": "reasoning_summary", "summary": "Checking assumptions first." }
            ]
        });

        assert_eq!(
            extract_codex_item_reasoning(&item),
            Some("Checking assumptions first.".to_string())
        );
    }

    #[test]
    fn extract_codex_reasoning_delta_text_accepts_reasoning_summary_aliases() {
        let payload = json!({
            "itemId": "abc",
            "reasoningSummary": {
                "output_text": "Need to confirm assumptions before edits."
            }
        });

        assert_eq!(
            extract_codex_reasoning_delta_text(&payload),
            Some("Need to confirm assumptions before edits.".to_string())
        );
    }

    #[test]
    fn extract_codex_reasoning_delta_text_accepts_legacy_event_msg_shape() {
        let payload = json!({
            "msg": {
                "type": "agent_reasoning_raw_content_delta",
                "delta": "Inspecting event payload shape."
            }
        });

        assert_eq!(
            extract_codex_reasoning_delta_text(&payload),
            Some("Inspecting event payload shape.".to_string())
        );
    }

    #[test]
    fn extract_codex_event_type_reads_legacy_method_suffix() {
        let payload = json!({});
        assert_eq!(
            extract_codex_event_type("codex/event/agent_reasoning_raw_content_delta", &payload),
            Some("agent_reasoning_raw_content_delta".to_string())
        );
    }

    #[test]
    fn extract_reasoning_delta_from_legacy_codex_event_parses_reasoning_delta() {
        let payload = json!({
            "msg": {
                "type": "agent_reasoning_raw_content_delta",
                "delta": "Evaluating alternatives."
            }
        });
        assert_eq!(
            extract_reasoning_delta_from_legacy_codex_event(
                "codex/event/agent_reasoning_raw_content_delta",
                &payload
            ),
            Some("Evaluating alternatives.".to_string())
        );
    }

    #[test]
    fn extract_reasoning_delta_from_legacy_codex_event_maps_section_break() {
        assert_eq!(
            extract_reasoning_delta_from_legacy_codex_event(
                "codex/event/agent_reasoning_section_break",
                &json!({})
            ),
            Some("\n\n".to_string())
        );
    }

    #[test]
    fn extract_reasoning_delta_from_legacy_codex_event_ignores_non_reasoning() {
        let payload = json!({
            "msg": {
                "type": "agent_message_delta",
                "delta": "Visible answer text."
            }
        });
        assert_eq!(
            extract_reasoning_delta_from_legacy_codex_event(
                "codex/event/agent_message_delta",
                &payload
            ),
            None
        );
    }

    #[test]
    fn is_codex_event_reasoning_type_handles_supported_values() {
        assert!(is_codex_event_reasoning_type("agent_reasoning"));
        assert!(is_codex_event_reasoning_type(
            "agent_reasoning_raw_content_delta"
        ));
        assert!(!is_codex_event_reasoning_type("agent_message_delta"));
    }

    #[test]
    fn is_reasoning_notification_method_accepts_alias_shapes() {
        assert!(is_reasoning_notification_method(
            "item/reasoning/summaryTextDelta"
        ));
        assert!(is_reasoning_notification_method(
            "item/reasoningSummaryText/delta"
        ));
        assert!(is_reasoning_notification_method(
            "item/reasoning_summary_text/delta"
        ));
        assert!(is_reasoning_notification_method("item/thinking/textDelta"));
        assert!(!is_reasoning_notification_method("item/agentMessage/delta"));
    }

    #[test]
    fn codex_error_notifications_are_non_terminal_while_turn_is_active() {
        let state = test_codex_state();
        assert!(!is_terminal_codex_error_notification(
            &state,
            &json!({ "message": "Tool warning" })
        ));
    }

    #[test]
    fn codex_error_notifications_are_terminal_when_idle_or_explicitly_fatal() {
        let mut idle_state = test_codex_state();
        idle_state.active_turn_id = None;
        idle_state.active_stream = None;
        idle_state.pending_request = None;

        assert!(is_terminal_codex_error_notification(
            &idle_state,
            &json!({ "message": "Session failed" })
        ));

        let active_state = test_codex_state();
        assert!(is_terminal_codex_error_notification(
            &active_state,
            &json!({ "message": "Fatal turn error", "fatal": true })
        ));
        assert!(is_terminal_codex_error_notification(
            &active_state,
            &json!({ "message": "Fatal turn error", "recoverable": false })
        ));
    }

    #[test]
    fn extract_codex_item_reasoning_reads_reasoning_thread_item_summary() {
        let item = json!({
            "type": "reasoning",
            "id": "reasoning-item-1",
            "summary": ["Check constraints", "Then produce final answer"],
            "content": []
        });

        assert_eq!(
            extract_codex_item_reasoning(&item),
            Some("Check constraints\nThen produce final answer".to_string())
        );
    }

    #[test]
    fn normalize_token_usage_accepts_flat_snake_case_payloads() {
        let normalized = normalize_token_usage(&json!({
            "input_tokens": 120,
            "output_tokens": 80,
            "total_tokens": 200,
            "cached_prompt_tokens": 20,
            "cache_creation_input_tokens": 5,
            "reasoning_tokens": 7,
            "context_window": 200000
        }));

        assert_eq!(normalized["input_tokens"], json!(120));
        assert_eq!(normalized["output_tokens"], json!(80));
        assert_eq!(normalized["total_tokens"], json!(200));
        assert_eq!(normalized["cached_prompt_tokens"], json!(20));
        assert_eq!(normalized["cache_creation_input_tokens"], json!(5));
        assert_eq!(normalized["reasoning_tokens"], json!(7));
        assert_eq!(normalized["context_window"], json!(200000));
    }

    #[test]
    fn extract_turn_token_usage_reads_nested_turn_shape() {
        let payload = json!({
            "turn": {
                "id": "turn_123",
                "usage": {
                    "input_tokens": 90,
                    "output_tokens": 30,
                    "total_tokens": 120
                }
            }
        });

        let (turn_id, usage) = extract_turn_token_usage(&payload).expect("turn usage");
        assert_eq!(turn_id, "turn_123");
        assert_eq!(usage["input_tokens"], json!(90));
        assert_eq!(usage["output_tokens"], json!(30));
        assert_eq!(usage["total_tokens"], json!(120));
    }
}
