use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use tokio::sync::{mpsc, Mutex};
use tyde_protocol::protocol::{
    ChatEvent, ChatMessage, ContextBreakdown, FileInfo, ImageData, MessageSender, ModelInfo,
    ModelsListData, ModelsListEntry, ModuleSchemasData, OperationCancelledData, ProfilesListData,
    SessionMetadata, SessionSettingsData, SessionStartedData, SessionsListData, StreamEndData,
    StreamStartData, StreamTextDeltaData, SubprocessExitData, Task, TaskList, TaskStatus,
    TokenUsage, ToolExecutionCompletedData, ToolExecutionResult, ToolRequest, ToolRequestType,
    ToolUseData,
};

use crate::acp::{
    acp_mcp_servers_json, extract_message_id, extract_text_from_update, extract_tool_call_id,
    map_plan_status, normalize_update_type, parse_tool_call_completion, parse_tool_call_request,
    AcpBridge, AcpInbound, AcpSpawnSpec,
};
use crate::agent::CommandExecutor;
use crate::backends::transport::BackendTransport;
use crate::backends::tycode::ImageAttachment;
use crate::backends::types::{SessionCommand, StartupMcpServer};

const KIRO_AGENT_NAME: &str = "kiro";
const KIRO_ADMIN_SESSION_SUBDIR: &str = ".tyde/kiro-admin";
const KIRO_EPHEMERAL_SESSION_SUBDIR: &str = ".tyde/kiro-ephemeral";

#[derive(Clone)]
pub struct KiroCommandHandle {
    inner: Arc<KiroInner>,
}

impl KiroCommandHandle {
    pub async fn execute(&self, command: SessionCommand) -> Result<(), String> {
        self.inner.execute(command).await
    }
}

impl CommandExecutor for KiroCommandHandle {
    fn execute(
        &self,
        command: SessionCommand,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        Box::pin(KiroCommandHandle::execute(self, command))
    }
}

pub struct KiroSession {
    inner: Arc<KiroInner>,
}

impl KiroSession {
    pub async fn spawn(
        workspace_roots: &[String],
        transport: BackendTransport,
        startup_mcp_servers: &[StartupMcpServer],
        steering_content: Option<&str>,
    ) -> Result<(Self, mpsc::UnboundedReceiver<ChatEvent>), String> {
        Self::spawn_with_mode(
            workspace_roots,
            false,
            transport,
            startup_mcp_servers,
            steering_content,
        )
        .await
    }

    pub async fn spawn_ephemeral(
        workspace_roots: &[String],
        transport: BackendTransport,
        startup_mcp_servers: &[StartupMcpServer],
        steering_content: Option<&str>,
    ) -> Result<(Self, mpsc::UnboundedReceiver<ChatEvent>), String> {
        Self::spawn_with_mode(
            workspace_roots,
            true,
            transport,
            startup_mcp_servers,
            steering_content,
        )
        .await
    }

    async fn spawn_with_mode(
        workspace_roots: &[String],
        ephemeral: bool,
        transport: BackendTransport,
        startup_mcp_servers: &[StartupMcpServer],
        steering_content: Option<&str>,
    ) -> Result<(Self, mpsc::UnboundedReceiver<ChatEvent>), String> {
        let roots = resolve_kiro_session_roots(workspace_roots, &transport, ephemeral).await?;
        let acp_args: Vec<&str> = vec!["acp"];

        let mut spawn_spec = AcpSpawnSpec::new("Kiro ACP", "kiro-cli-chat", &acp_args)
            .with_local_cwd(roots.session_cwd.clone());
        spawn_spec.local_program = resolve_kiro_chat_binary();

        let (bridge, inbound_rx) = AcpBridge::spawn(spawn_spec, transport.clone()).await?;

        bridge
            .request(
                "initialize",
                json!({
                    "protocolVersion": 1,
                    "clientCapabilities": {
                        "fs": {
                            "readTextFile": true,
                            "writeTextFile": true
                        },
                        "terminal": true
                    },
                    "clientInfo": {
                        "name": "tyde",
                        "title": "Tyde",
                        "version": "0.1.0"
                    }
                }),
            )
            .await?;

        let session_result: Result<(String, Value), String> = async {
            let mut session_params = json!({
                "cwd": roots.session_cwd,
                "mcpServers": acp_mcp_servers_json(startup_mcp_servers)
            });
            if let Some(content) = steering_content {
                if !content.trim().is_empty() {
                    session_params["systemPrompt"] = Value::String(content.to_string());
                }
            }
            let session_started = bridge.request("session/new", session_params).await?;

            let session_id = session_started
                .get("sessionId")
                .and_then(Value::as_str)
                .or_else(|| {
                    session_started
                        .get("session")
                        .and_then(|v| v.get("sessionId"))
                        .and_then(Value::as_str)
                })
                .ok_or("Kiro session/new response missing sessionId")?
                .to_string();

            Ok((session_id, session_started))
        }
        .await;

        let (session_id, session_started) = session_result?;

        let initial_model = extract_current_model(&session_started);
        let initial_mode = extract_current_mode(&session_started);

        let (event_tx, event_rx) = mpsc::unbounded_channel();

        let inner = Arc::new(KiroInner {
            bridge,
            event_tx,
            shutting_down: AtomicBool::new(false),
            transport,
            state: Mutex::new(KiroState {
                session_id,
                workspace_root: roots.scope_root,
                admin_session: false,
                steering_content: steering_content.map(|s| s.to_string()),
                startup_mcp_servers: startup_mcp_servers.to_vec(),
                model: initial_model,
                mode: initial_mode,
                known_models: extract_known_models(&session_started),
                active_message_id: None,
                active_stream_text: String::new(),
                active_stream_tool_calls: Vec::new(),
                active_tool_contexts: HashMap::new(),
                tool_call_aliases: HashMap::new(),
                cancelled: false,
                replaying_history: false,
                replay_user_message_id: None,
                replay_user_text: String::new(),
                replay_assistant_message_id: None,
                replay_assistant_text: String::new(),
                replay_assistant_tool_calls: Vec::new(),
                replay_tool_contexts: HashMap::new(),
                replay_tool_completion_order: Vec::new(),
                replay_message_ids: HashSet::new(),
                replay_tool_call_ids: HashSet::new(),
            }),
        });

        let forward_inner = Arc::clone(&inner);
        tokio::spawn(async move {
            let mut rx = inbound_rx;
            while let Some(msg) = rx.recv().await {
                forward_inner.handle_inbound(msg).await;
            }
        });

        // Emit SessionStarted so forward_events sets backend_session_id on the store record
        {
            let state = inner.state.lock().await;
            let _ = inner
                .event_tx
                .send(ChatEvent::SessionStarted(SessionStartedData {
                    session_id: state.session_id.clone(),
                }));
        }

        Ok((Self { inner }, event_rx))
    }

    pub fn command_handle(&self) -> KiroCommandHandle {
        KiroCommandHandle {
            inner: Arc::clone(&self.inner),
        }
    }

    pub async fn shutdown(self) {
        self.inner.shutdown().await;
    }
}

struct KiroState {
    session_id: String,
    workspace_root: String,
    admin_session: bool,
    steering_content: Option<String>,
    startup_mcp_servers: Vec<StartupMcpServer>,
    model: Option<String>,
    mode: Option<String>,
    known_models: Vec<ModelsListEntry>,
    active_message_id: Option<String>,
    active_stream_text: String,
    active_stream_tool_calls: Vec<ToolUseData>,
    active_tool_contexts: HashMap<String, KiroToolContext>,
    tool_call_aliases: HashMap<String, String>,
    cancelled: bool,
    replaying_history: bool,
    replay_user_message_id: Option<String>,
    replay_user_text: String,
    replay_assistant_message_id: Option<String>,
    replay_assistant_text: String,
    replay_assistant_tool_calls: Vec<ToolUseData>,
    replay_tool_contexts: HashMap<String, KiroReplayToolContext>,
    replay_tool_completion_order: Vec<String>,
    replay_message_ids: HashSet<String>,
    replay_tool_call_ids: HashSet<String>,
}

#[derive(Clone)]
struct PendingToolCompletion {
    tool_name: String,
    tool_result: ToolExecutionResult,
    success: bool,
    error: Option<String>,
}

#[derive(Clone)]
struct KiroToolContext {
    tool_name: String,
    tool_type: ToolRequestType,
    request_emitted: bool,
    pending_completion: Option<PendingToolCompletion>,
}

struct KiroReplayToolContext {
    tool_name: String,
    tool_type: ToolRequestType,
    completion: Option<PendingToolCompletion>,
}

struct KiroInner {
    bridge: AcpBridge,
    event_tx: mpsc::UnboundedSender<ChatEvent>,
    state: Mutex<KiroState>,
    shutting_down: AtomicBool,
    transport: BackendTransport,
}

impl KiroInner {
    async fn execute(&self, command: SessionCommand) -> Result<(), String> {
        match command {
            SessionCommand::SendMessage { message, images } => {
                self.prepare_for_live_prompt().await;
                self.emit_user_message_added(&message, images.as_deref());
                self.emit_event(ChatEvent::TypingStatusChanged(true));

                let (session_id, model, mode, steering) = {
                    let state = self.state.lock().await;
                    (
                        state.session_id.clone(),
                        state.model.clone(),
                        state.mode.clone(),
                        state.steering_content.clone(),
                    )
                };

                let effective_message = if let Some(ref s) = steering {
                    format!("{}\n\n{}", s, message)
                } else {
                    message.clone()
                };

                let mut prompt_blocks = vec![json!({
                    "type": "text",
                    "text": effective_message,
                })];

                if let Some(imgs) = images {
                    for image in imgs {
                        prompt_blocks.push(json!({
                            "type": "image",
                            "mimeType": image.media_type,
                            "data": image.data,
                        }));
                    }
                }

                let mut params = json!({
                    "sessionId": session_id,
                    "prompt": prompt_blocks,
                });

                if let Some(model_id) = model {
                    params["modelId"] = Value::String(model_id);
                }
                if let Some(mode_id) = mode {
                    params["modeId"] = Value::String(mode_id);
                }
                if let Some(ref s) = steering {
                    params["systemPrompt"] = Value::String(s.clone());
                }

                self.state.lock().await.cancelled = false;

                let response = match self.bridge.request("session/prompt", params).await {
                    Ok(value) => value,
                    Err(err) => {
                        // CancelConversation sets `cancelled = true` before sending
                        // session/cancel. If the prompt error is just the stale
                        // rejection of a cancelled request, surface it as a normal
                        // cancellation instead of a hard backend failure.
                        let mut state = self.state.lock().await;
                        if state.cancelled {
                            state.cancelled = false;
                            drop(state);
                            self.clear_active_stream().await;
                            self.emit_event(ChatEvent::TypingStatusChanged(false));
                            self.emit_event(ChatEvent::OperationCancelled(
                                OperationCancelledData {
                                    message: "Operation cancelled".to_string(),
                                },
                            ));
                            return Ok(());
                        }
                        drop(state);
                        self.emit_event(ChatEvent::TypingStatusChanged(false));
                        return Err(err);
                    }
                };

                if let Some(model) = extract_current_model(&response) {
                    let mut state = self.state.lock().await;
                    state.model = Some(model);
                }
                if let Some(mode) = extract_current_mode(&response) {
                    let mut state = self.state.lock().await;
                    state.mode = Some(mode);
                }
                let known_models = extract_known_models(&response);
                if !known_models.is_empty() {
                    let mut state = self.state.lock().await;
                    state.known_models = known_models;
                }

                let stop_reason = response
                    .get("stopReason")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_ascii_lowercase();

                {
                    let mut state = self.state.lock().await;
                    state.cancelled = false;
                }

                if stop_reason == "cancelled" {
                    self.clear_active_stream().await;
                    self.emit_event(ChatEvent::TypingStatusChanged(false));
                    self.emit_event(ChatEvent::OperationCancelled(OperationCancelledData {
                        message: "Operation cancelled".to_string(),
                    }));
                    return Ok(());
                }

                if stop_reason == "failed" || stop_reason == "error" {
                    let message = response
                        .get("error")
                        .and_then(|v| v.get("message"))
                        .and_then(Value::as_str)
                        .or_else(|| response.get("message").and_then(Value::as_str))
                        .unwrap_or("Kiro prompt failed")
                        .to_string();
                    self.clear_active_stream().await;
                    self.emit_event(ChatEvent::TypingStatusChanged(false));
                    self.emit_event(ChatEvent::Error(message));
                    return Ok(());
                }

                self.finalize_active_stream_if_any(Some(response), true)
                    .await;
                Ok(())
            }
            SessionCommand::CancelConversation => {
                let mut state = self.state.lock().await;
                state.cancelled = true;
                let session_id = state.session_id.clone();
                drop(state);
                self.bridge
                    .notify("session/cancel", json!({ "sessionId": session_id }))
                    .await?;
                Ok(())
            }
            SessionCommand::GetSettings => {
                let state = self.state.lock().await;
                self.emit_event(ChatEvent::Settings(SessionSettingsData {
                    model: Some(state.model.clone()),
                    mode: Some(state.mode.clone()),
                    ..SessionSettingsData::default()
                }));
                Ok(())
            }
            SessionCommand::ListSessions => self.list_sessions().await,
            SessionCommand::ResumeSession { session_id } => self.resume_session(session_id).await,
            SessionCommand::DeleteSession { session_id } => self.delete_session(session_id).await,
            SessionCommand::ListProfiles => {
                self.emit_event(ChatEvent::ProfilesList(ProfilesListData {
                    profiles: Vec::new(),
                }));
                Ok(())
            }
            SessionCommand::SwitchProfile { profile_name: _ } => Ok(()),
            SessionCommand::GetModuleSchemas => {
                self.emit_event(ChatEvent::ModuleSchemas(ModuleSchemasData {
                    schemas: Vec::new(),
                }));
                Ok(())
            }
            SessionCommand::ListModels => {
                let models = self.state.lock().await.known_models.clone();
                self.emit_event(ChatEvent::ModelsList(ModelsListData { models }));
                Ok(())
            }
            SessionCommand::UpdateSettings {
                settings,
                persist: _,
            } => {
                if let Some(model_value) = settings.model {
                    let next_model = model_value.and_then(|model| {
                        let normalized = model.trim();
                        if normalized.is_empty() {
                            None
                        } else {
                            Some(normalized.to_string())
                        }
                    });
                    let session_id = self.state.lock().await.session_id.clone();
                    match next_model.clone() {
                        Some(model_id) => {
                            self.bridge
                                .request(
                                    "session/set_model",
                                    json!({
                                        "sessionId": session_id,
                                        "modelId": model_id,
                                        "model": model_id,
                                    }),
                                )
                                .await?;
                        }
                        None => {
                            // Let backend fallback to default model.
                        }
                    }
                    let mut state = self.state.lock().await;
                    state.model = next_model;
                }

                if let Some(mode_value) = settings.mode {
                    let next_mode = mode_value.and_then(|mode| {
                        let normalized = mode.trim();
                        if normalized.is_empty() {
                            None
                        } else {
                            Some(normalized.to_string())
                        }
                    });
                    let session_id = self.state.lock().await.session_id.clone();
                    if let Some(mode_id) = next_mode.clone() {
                        self.bridge
                            .request(
                                "session/set_mode",
                                json!({
                                    "sessionId": session_id,
                                    "modeId": mode_id,
                                    "mode": mode_id,
                                }),
                            )
                            .await?;
                    }
                    let mut state = self.state.lock().await;
                    state.mode = next_mode;
                }

                let state = self.state.lock().await;
                self.emit_event(ChatEvent::Settings(SessionSettingsData {
                    model: Some(state.model.clone()),
                    mode: Some(state.mode.clone()),
                    ..SessionSettingsData::default()
                }));
                Ok(())
            }
        }
    }

    async fn list_sessions(&self) -> Result<(), String> {
        let excluded_session_id = {
            let state = self.state.lock().await;
            if state.admin_session {
                Some(state.session_id.clone())
            } else {
                None
            }
        };

        let raw_sessions = load_kiro_sessions_for_transport(&self.transport).await?;

        let mut sessions = Vec::new();
        for (session_id, metadata) in &raw_sessions {
            if excluded_session_id.as_deref() == Some(session_id.as_str()) {
                continue;
            }
            let cwd = metadata
                .get("cwd")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if cwd.contains(KIRO_ADMIN_SESSION_SUBDIR)
                || cwd.contains(KIRO_EPHEMERAL_SESSION_SUBDIR)
            {
                continue;
            }
            let title = extract_session_title(metadata);
            let last_modified = extract_session_timestamp(metadata);

            sessions.push(SessionMetadata {
                id: session_id.clone(),
                session_id: Some(session_id.to_string()),
                title,
                created_at: Some(last_modified),
                last_modified,
                message_count: None,
                last_message_preview: Some(String::new()),
                preview: Some(String::new()),
                workspace_root: Some(cwd.to_string()),
                backend_kind: Some("kiro".to_string()),
            });
        }

        sessions.sort_by(|a, b| b.last_modified.cmp(&a.last_modified));

        self.emit_event(ChatEvent::SessionsList(SessionsListData { sessions }));
        Ok(())
    }

    async fn delete_session(&self, session_id: String) -> Result<(), String> {
        let normalized = normalize_optional_string(&Value::String(session_id))
            .ok_or("Invalid session id".to_string())?;

        delete_kiro_session_for_transport(&self.transport, &normalized).await
    }

    async fn resume_session(&self, session_id: String) -> Result<(), String> {
        let normalized = normalize_optional_string(&Value::String(session_id))
            .ok_or("Invalid session id".to_string())?;
        let (cwd, startup_mcp_servers) = {
            let mut state = self.state.lock().await;
            state.replaying_history = true;
            state.replay_user_message_id = None;
            state.replay_user_text.clear();
            state.replay_assistant_message_id = None;
            state.replay_assistant_text.clear();
            state.replay_assistant_tool_calls.clear();
            state.replay_tool_contexts.clear();
            state.replay_tool_completion_order.clear();
            state.replay_message_ids.clear();
            state.replay_tool_call_ids.clear();
            (
                state.workspace_root.clone(),
                state.startup_mcp_servers.clone(),
            )
        };

        self.clear_active_stream().await;
        self.emit_event(ChatEvent::ConversationCleared);
        self.emit_event(ChatEvent::TypingStatusChanged(false));

        // kiro-cli-chat doesn't check PID liveness when reading .lock files,
        // so stale locks from dead processes block session/load. Remove the
        // lock file before attempting to load.
        let _ = clear_kiro_session_lock_for_transport(&self.transport, &normalized).await;

        let response = match self
            .bridge
            .request(
                "session/load",
                json!({
                    "sessionId": &normalized,
                    "cwd": cwd,
                    "mcpServers": acp_mcp_servers_json(&startup_mcp_servers),
                }),
            )
            .await
        {
            Ok(response) => response,
            Err(err) => {
                let mut state = self.state.lock().await;
                state.replaying_history = false;
                state.replay_user_message_id = None;
                state.replay_user_text.clear();
                state.replay_assistant_message_id = None;
                state.replay_assistant_text.clear();
                state.replay_assistant_tool_calls.clear();
                state.replay_tool_contexts.clear();
                state.replay_tool_completion_order.clear();
                state.replay_message_ids.clear();
                state.replay_tool_call_ids.clear();
                self.emit_event(ChatEvent::TypingStatusChanged(false));
                return Err(normalize_kiro_resume_error(err));
            }
        };

        {
            let mut state = self.state.lock().await;
            state.session_id = normalized.clone();
            if let Some(model) = extract_current_model(&response) {
                state.model = Some(model);
            }
            if let Some(mode) = extract_current_mode(&response) {
                state.mode = Some(mode);
            }
            let known_models = extract_known_models(&response);
            if !known_models.is_empty() {
                state.known_models = known_models;
            }

            self.emit_event(ChatEvent::SessionStarted(SessionStartedData {
                session_id: state.session_id.clone(),
            }));
        }
        self.flush_replay_history().await;
        self.emit_event(ChatEvent::TypingStatusChanged(false));
        Ok(())
    }

    async fn shutdown(&self) {
        self.shutting_down.store(true, Ordering::Release);
        self.bridge.shutdown().await;
    }

    async fn handle_inbound(&self, inbound: AcpInbound) {
        match inbound {
            AcpInbound::Stderr(line) => {
                self.emit_event(ChatEvent::SubprocessStderr(line));
            }
            AcpInbound::Closed { exit_code } => {
                let code = if self.shutting_down.load(Ordering::Acquire) {
                    Some(0)
                } else {
                    exit_code
                };
                self.emit_event(ChatEvent::SubprocessExit(SubprocessExitData {
                    exit_code: code,
                }));
            }
            AcpInbound::Notification { method, params } => {
                self.handle_notification(&method, &params).await;
            }
            AcpInbound::ServerRequest { id, method, params } => {
                match self
                    .bridge
                    .handle_server_request(id.clone(), &method, &params)
                    .await
                {
                    Ok(true) => {}
                    Ok(false) => {
                        let _ = self.bridge.respond(id, json!({ "ignored": true })).await;
                    }
                    Err(err) => {
                        self.emit_event(ChatEvent::SubprocessStderr(format!(
                            "Failed to handle server request '{method}': {err}"
                        )));
                        let _ = self.bridge.respond_error(id, -32_000, &err).await;
                    }
                }
            }
        }
    }
    async fn handle_notification(&self, method: &str, params: &Value) {
        match method {
            "session/notification" => {
                self.handle_kiro_notification(params).await;
            }
            "session/update" => {
                self.handle_standard_update(params).await;
            }
            _ => {}
        }
    }

    async fn handle_kiro_notification(&self, params: &Value) {
        let raw_type = params
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let normalized = normalize_update_type(raw_type);

        match normalized.as_str() {
            "agentmessagechunk" => {
                self.handle_agent_message_chunk(params).await;
            }
            "toolcall" => {
                self.handle_tool_call(params).await;
            }
            "toolcallupdate" => {
                self.handle_tool_call_update(params).await;
            }
            "turnend" => {
                if self.state.lock().await.replaying_history {
                    self.flush_replay_history().await;
                    return;
                }
                self.finalize_active_stream_if_any(Some(params.clone()), true)
                    .await;
            }
            "error" => {
                self.handle_error_notification(params).await;
            }
            "currentmodeupdate" => {
                if let Some(mode) = extract_current_mode(params) {
                    let mut state = self.state.lock().await;
                    state.mode = Some(mode);
                }
            }
            "configoptionupdate" => {
                if let Some(model) = extract_current_model(params) {
                    let mut state = self.state.lock().await;
                    state.model = Some(model);
                }
                let models = extract_known_models(params);
                if !models.is_empty() {
                    let mut state = self.state.lock().await;
                    state.known_models = models;
                }
            }
            _ => {}
        }
    }

    async fn handle_standard_update(&self, params: &Value) {
        let update = params.get("update").unwrap_or(params);
        let update_type = update
            .get("sessionUpdate")
            .or_else(|| update.get("session_update"))
            .and_then(Value::as_str)
            .unwrap_or_default();

        match update_type {
            "user_message_chunk" => {
                self.handle_user_message_chunk(update).await;
            }
            "agent_message_chunk" => {
                self.handle_agent_message_chunk(update).await;
            }
            "agent_thought_chunk" => self.handle_reasoning_chunk(update).await,
            "tool_call" => self.handle_tool_call(update).await,
            "tool_call_update" => self.handle_tool_call_update(update).await,
            "plan" => {
                self.handle_plan_update(update);
            }
            "current_mode_update" => {
                if let Some(mode) = extract_current_mode(update) {
                    let mut state = self.state.lock().await;
                    state.mode = Some(mode);
                }
            }
            "config_option_update" => {
                if let Some(model) = extract_current_model(update) {
                    let mut state = self.state.lock().await;
                    state.model = Some(model);
                }
                let models = extract_known_models(update);
                if !models.is_empty() {
                    let mut state = self.state.lock().await;
                    state.known_models = models;
                }
            }
            _ => {}
        }
    }

    async fn handle_user_message_chunk(&self, params: &Value) {
        let raw_text = extract_text_from_update(params);
        if raw_text.is_empty() {
            return;
        }

        let message_id = extract_kiro_message_id(params);
        let replaying_history = {
            let state = self.state.lock().await;
            if state.replaying_history {
                true
            } else if is_suppressed_history_message(&state, message_id.as_deref()) {
                return;
            } else {
                false
            }
        };

        if !replaying_history {
            return;
        }

        let steering_content = self.state.lock().await.steering_content.clone();
        let text = strip_kiro_steering_prefix(&raw_text, steering_content.as_deref())
            .trim()
            .to_string();
        if text.is_empty() {
            return;
        }

        self.flush_replay_assistant_message().await;

        let previous_user = {
            let mut state = self.state.lock().await;
            if let Some(next_id) = message_id.as_deref() {
                state.replay_message_ids.insert(next_id.to_string());
            }
            let previous = if let Some(next_id) = message_id.as_deref() {
                if state.replay_user_message_id.as_deref() != Some(next_id)
                    && has_visible_text(&state.replay_user_text)
                {
                    Some(std::mem::take(&mut state.replay_user_text))
                } else {
                    None
                }
            } else {
                None
            };
            if let Some(next_id) = message_id {
                state.replay_user_message_id = Some(next_id);
            }
            state.replay_user_text.push_str(&text);
            previous
        };

        if let Some(previous_user) = previous_user {
            self.emit_replay_user_message(previous_user).await;
        }
    }

    async fn handle_reasoning_chunk(&self, params: &Value) {
        let delta = extract_text_from_update(params);
        if delta.trim().is_empty() {
            return;
        }
        let incoming_message_id = extract_kiro_message_id(params);

        let message_id = {
            let mut state = self.state.lock().await;
            if state.replaying_history
                || is_suppressed_history_message(&state, incoming_message_id.as_deref())
            {
                return;
            }
            if let Some(id) = incoming_message_id {
                if state.active_message_id.is_none() {
                    state.active_message_id = Some(id.clone());
                    state.active_stream_text.clear();
                    state.active_stream_tool_calls.clear();
                    let model = state.model.clone().unwrap_or_else(|| "kiro".to_string());
                    self.emit_event(ChatEvent::TypingStatusChanged(true));
                    self.emit_event(ChatEvent::StreamStart(StreamStartData {
                        message_id: Some(id),
                        agent: KIRO_AGENT_NAME.to_string(),
                        model: Some(model),
                    }));
                }
            }
            state
                .active_message_id
                .clone()
                .unwrap_or_else(|| format!("kiro-msg-{}", unix_now_ms()))
        };

        self.emit_event(ChatEvent::StreamReasoningDelta(StreamTextDeltaData {
            message_id: Some(message_id),
            text: delta.to_string(),
        }));
    }

    async fn handle_agent_message_chunk(&self, params: &Value) {
        let raw_delta = extract_text_from_update(params);
        if raw_delta.is_empty() {
            return;
        }
        let delta = strip_ansi_and_controls(&raw_delta);
        if delta.is_empty() {
            return;
        }

        let chunk_message_id = extract_kiro_message_id(params);
        let replaying_history = {
            let state = self.state.lock().await;
            if state.replaying_history {
                true
            } else if is_suppressed_history_message(&state, chunk_message_id.as_deref()) {
                return;
            } else {
                false
            }
        };

        if replaying_history {
            self.handle_replay_agent_message_chunk(chunk_message_id, delta)
                .await;
            return;
        }

        if !has_renderable_stream_text(&delta) {
            let has_active_stream = self.state.lock().await.active_message_id.is_some();
            if !has_active_stream {
                return;
            }
        }

        let (previous_stream, started_message_id, model, stream_message_id) = {
            let mut state = self.state.lock().await;
            let mut previous_stream: Option<(String, String, Vec<ToolUseData>)> = None;

            if let Some(next_id) = chunk_message_id.clone() {
                if let Some(active_id) = state.active_message_id.clone() {
                    if active_id != next_id {
                        let previous_text = std::mem::take(&mut state.active_stream_text);
                        let previous_tool_calls =
                            std::mem::take(&mut state.active_stream_tool_calls);
                        if has_renderable_stream_text(&previous_text)
                            || !previous_tool_calls.is_empty()
                        {
                            previous_stream = Some((active_id, previous_text, previous_tool_calls));
                        }
                        state.active_message_id = None;
                    }
                }
            }

            let message_id = state.active_message_id.clone().unwrap_or_else(|| {
                chunk_message_id
                    .clone()
                    .unwrap_or_else(|| format!("kiro-msg-{}", unix_now_ms()))
            });

            let started = if state.active_message_id.is_none() {
                state.active_message_id = Some(message_id.clone());
                state.active_stream_text.clear();
                state.active_stream_tool_calls.clear();
                Some(message_id.clone())
            } else {
                None
            };

            state.active_stream_text.push_str(&delta);

            (
                previous_stream,
                started,
                state.model.clone().unwrap_or_else(|| "kiro".to_string()),
                message_id,
            )
        };

        if let Some((prev_message_id, prev_text, prev_tool_calls)) = previous_stream {
            self.emit_stream_end(prev_message_id, prev_text, None, prev_tool_calls, false)
                .await;
        }

        if let Some(start_message_id) = started_message_id {
            self.emit_event(ChatEvent::TypingStatusChanged(true));
            self.emit_event(ChatEvent::StreamStart(StreamStartData {
                message_id: Some(start_message_id),
                agent: KIRO_AGENT_NAME.to_string(),
                model: Some(model),
            }));
        }

        self.emit_event(ChatEvent::StreamDelta(StreamTextDeltaData {
            message_id: Some(stream_message_id),
            text: delta.to_string(),
        }));
    }

    async fn handle_tool_call(&self, params: &Value) {
        let Some(request) = parse_tool_call_request(params) else {
            self.emit_event(ChatEvent::SubprocessStderr(format!(
                "Ignoring ACP tool_call without toolCallId: {params}"
            )));
            return;
        };
        let raw_tool_call_id = normalize_tool_call_id_fragment(&request.tool_call_id);

        let incoming_message_id = extract_kiro_message_id(params);
        let replaying_history = {
            let state = self.state.lock().await;
            if state.replaying_history {
                true
            } else if is_suppressed_history_tool_event(
                &state,
                Some(raw_tool_call_id.as_str()),
                incoming_message_id.as_deref(),
            ) {
                return;
            } else {
                false
            }
        };

        if replaying_history {
            self.handle_replay_tool_call(params, request, raw_tool_call_id, incoming_message_id)
                .await;
            return;
        }
        let workspace_root = self.state.lock().await.workspace_root.clone();

        let mut start_event: Option<(String, String)> = None;
        let mut previous_stream: Option<(String, String, Vec<ToolUseData>)> = None;
        let mut should_finalize_current = false;
        let mut refresh_tool_request: Option<(String, String, ToolRequestType)> = None;
        {
            let mut state = self.state.lock().await;

            if let Some(next_id) = incoming_message_id.clone() {
                if let Some(active_id) = state.active_message_id.clone() {
                    if active_id != next_id {
                        let previous_text = std::mem::take(&mut state.active_stream_text);
                        let previous_tool_calls =
                            std::mem::take(&mut state.active_stream_tool_calls);
                        if has_renderable_stream_text(&previous_text)
                            || !previous_tool_calls.is_empty()
                        {
                            previous_stream = Some((active_id, previous_text, previous_tool_calls));
                        }
                        state.active_message_id = None;
                    }
                }
            }

            let stream_message_id = incoming_message_id
                .clone()
                .or_else(|| state.active_message_id.clone())
                .unwrap_or_else(|| format!("kiro-msg-{}", unix_now_ms()));

            let canonical_id =
                build_canonical_tool_call_id(&mut state, &stream_message_id, &raw_tool_call_id);
            let duplicate_request = state.active_tool_contexts.contains_key(&canonical_id);
            let tool_type = map_tool_request_type(params, &request.args, &workspace_root).await;

            let context = state
                .active_tool_contexts
                .entry(canonical_id.clone())
                .or_insert_with(|| KiroToolContext {
                    tool_name: request.tool_name.clone(),
                    tool_type: tool_type.clone(),
                    request_emitted: false,
                    pending_completion: None,
                });
            let prev_tool_type = context.tool_type.clone();
            let request_already_emitted = context.request_emitted;
            context.tool_type = tool_type.clone();

            if duplicate_request && request_already_emitted {
                let changed = prev_tool_type != tool_type;
                if changed {
                    refresh_tool_request = Some((
                        canonical_id.clone(),
                        request.tool_name.clone(),
                        tool_type.clone(),
                    ));
                }
            }

            state
                .tool_call_aliases
                .insert(tool_alias_raw_key(&raw_tool_call_id), canonical_id.clone());
            state.tool_call_aliases.insert(
                tool_alias_message_key(&stream_message_id, &raw_tool_call_id),
                canonical_id.clone(),
            );

            if !duplicate_request {
                if state.active_message_id.is_none() {
                    state.active_message_id = Some(stream_message_id.clone());
                    state.active_stream_text.clear();
                    state.active_stream_tool_calls.clear();
                    let model = state.model.clone().unwrap_or_else(|| "kiro".to_string());
                    start_event = Some((stream_message_id.clone(), model));
                }

                let tool_call_entry = ToolUseData {
                    id: canonical_id.clone(),
                    name: request.tool_name.clone(),
                    arguments: request.args.clone(),
                };
                let already_present = state
                    .active_stream_tool_calls
                    .iter()
                    .any(|call| call.id == canonical_id);
                if !already_present {
                    state.active_stream_tool_calls.push(tool_call_entry);
                }
                should_finalize_current = true;
            }
        };

        if let Some((prev_message_id, prev_text, prev_tool_calls)) = previous_stream {
            self.emit_stream_end(prev_message_id, prev_text, None, prev_tool_calls, false)
                .await;
        }

        if let Some((message_id, model)) = start_event {
            self.emit_event(ChatEvent::TypingStatusChanged(true));
            self.emit_event(ChatEvent::StreamStart(StreamStartData {
                message_id: Some(message_id),
                agent: KIRO_AGENT_NAME.to_string(),
                model: Some(model),
            }));
        }

        if should_finalize_current {
            self.finalize_active_stream_if_any(None, false).await;
        }

        if let Some((tool_call_id, tool_name, tool_type)) = refresh_tool_request {
            self.emit_event(ChatEvent::ToolRequest(ToolRequest {
                tool_call_id,
                tool_name,
                tool_type,
            }));
        }
    }

    async fn handle_tool_call_update(&self, params: &Value) {
        let raw_tool_call_id =
            extract_kiro_tool_call_id(params).map(|raw| normalize_tool_call_id_fragment(&raw));
        let message_id = extract_kiro_message_id(params);

        let replaying_history = {
            let state = self.state.lock().await;
            if state.replaying_history {
                true
            } else if is_suppressed_history_tool_event(
                &state,
                raw_tool_call_id.as_deref(),
                message_id.as_deref(),
            ) {
                return;
            } else {
                false
            }
        };

        if replaying_history {
            self.handle_replay_tool_call_update(params, raw_tool_call_id, message_id)
                .await;
            return;
        }

        let (resolved_tool_call_id, fallback_name) = {
            let state = self.state.lock().await;
            let resolved_id = resolve_tool_call_id_alias(
                &state,
                raw_tool_call_id.as_deref(),
                message_id.as_deref(),
            );
            let fallback_name = resolved_id
                .as_ref()
                .and_then(|id| state.active_tool_contexts.get(id))
                .map(|ctx| ctx.tool_name.clone());
            (resolved_id, fallback_name)
        };

        let Some(resolved_tool_call_id) = resolved_tool_call_id else {
            return;
        };
        let Some(mut completion) = parse_tool_call_completion(params, fallback_name) else {
            return;
        };
        completion.tool_call_id = resolved_tool_call_id;

        let backfill_after_path = {
            let state = self.state.lock().await;
            if !completion.success {
                None
            } else if let Some(context) = state.active_tool_contexts.get(&completion.tool_call_id) {
                match &context.tool_type {
                    ToolRequestType::ModifyFile {
                        file_path,
                        before,
                        after,
                    } => {
                        if file_path.is_empty()
                            || !has_visible_text(before)
                            || has_visible_text(after)
                        {
                            None
                        } else {
                            let resolved = resolve_tool_file_path(file_path, &state.workspace_root);
                            if resolved.is_empty() || !Path::new(&resolved).exists() {
                                None
                            } else {
                                Some(resolved)
                            }
                        }
                    }
                    _ => None,
                }
            } else {
                None
            }
        };

        let backfilled_after_contents = if let Some(path) = backfill_after_path {
            tokio::fs::read_to_string(&path)
                .await
                .ok()
                .filter(|contents| has_visible_text(contents))
        } else {
            None
        };

        let mut emit_completion_now: Option<(
            String,
            String,
            ToolExecutionResult,
            bool,
            Option<String>,
        )> = None;
        let mut refresh_tool_request: Option<(String, String, ToolRequestType)> = None;
        {
            let mut state = self.state.lock().await;
            if let Some(context) = state.active_tool_contexts.get_mut(&completion.tool_call_id) {
                if let Some(after_contents) = backfilled_after_contents.clone() {
                    let needs_update = match &context.tool_type {
                        ToolRequestType::ModifyFile { after, .. } => after != &after_contents,
                        _ => false,
                    };
                    if needs_update {
                        if let ToolRequestType::ModifyFile { after, .. } = &mut context.tool_type {
                            *after = after_contents;
                        }
                        if context.request_emitted {
                            refresh_tool_request = Some((
                                completion.tool_call_id.clone(),
                                context.tool_name.clone(),
                                context.tool_type.clone(),
                            ));
                        }
                    }
                }

                if completion.tool_name == "tool" {
                    completion.tool_name = context.tool_name.clone();
                }
                let tool_result = map_tool_completion_result(&completion, Some(context));
                let pending = PendingToolCompletion {
                    tool_name: completion.tool_name.clone(),
                    tool_result,
                    success: completion.success,
                    error: completion.error.clone(),
                };
                if context.request_emitted {
                    emit_completion_now = Some((
                        completion.tool_call_id.clone(),
                        pending.tool_name,
                        pending.tool_result,
                        pending.success,
                        pending.error,
                    ));
                } else {
                    context.pending_completion = Some(pending);
                }
            } else {
                return;
            }

            if emit_completion_now.is_some() {
                state.active_tool_contexts.remove(&completion.tool_call_id);
                remove_tool_call_aliases(
                    &mut state.tool_call_aliases,
                    &completion.tool_call_id,
                    raw_tool_call_id.as_deref(),
                    message_id.as_deref(),
                );
            }
        }

        if let Some((tool_call_id, tool_name, tool_type)) = refresh_tool_request {
            self.emit_event(ChatEvent::ToolRequest(ToolRequest {
                tool_call_id,
                tool_name,
                tool_type,
            }));
        }

        if let Some((tool_call_id, tool_name, tool_result, success, error)) = emit_completion_now {
            self.emit_event(ChatEvent::ToolExecutionCompleted(
                ToolExecutionCompletedData {
                    tool_call_id,
                    tool_name,
                    tool_result,
                    success,
                    error,
                },
            ));
        }
    }

    async fn prepare_for_live_prompt(&self) {
        self.flush_replay_history().await;
        let mut state = self.state.lock().await;
        state.replaying_history = false;
        state.replay_user_message_id = None;
        state.replay_user_text.clear();
        state.replay_assistant_message_id = None;
        state.replay_assistant_text.clear();
        state.replay_assistant_tool_calls.clear();
        state.replay_tool_contexts.clear();
        state.replay_tool_completion_order.clear();
    }

    async fn handle_replay_agent_message_chunk(&self, message_id: Option<String>, delta: String) {
        if !has_renderable_stream_text(&delta) {
            let has_active_replay_message = {
                let state = self.state.lock().await;
                has_visible_text(&state.replay_assistant_text)
                    || !state.replay_assistant_tool_calls.is_empty()
            };
            if !has_active_replay_message {
                return;
            }
        }

        self.flush_replay_user_message().await;

        let flushed = {
            let mut state = self.state.lock().await;
            let should_flush = if !state.replay_assistant_tool_calls.is_empty() {
                true
            } else if let Some(next_id) = message_id.as_deref() {
                state.replay_assistant_message_id.as_deref() != Some(next_id)
                    && (has_visible_text(&state.replay_assistant_text)
                        || !state.replay_assistant_tool_calls.is_empty())
            } else {
                false
            };

            let flushed = if should_flush {
                Some((
                    std::mem::take(&mut state.replay_assistant_text),
                    std::mem::take(&mut state.replay_assistant_tool_calls),
                    std::mem::take(&mut state.replay_tool_contexts),
                    std::mem::take(&mut state.replay_tool_completion_order),
                ))
            } else {
                None
            };

            if should_flush {
                state.replay_assistant_message_id = None;
            }
            if let Some(next_id) = message_id {
                state.replay_message_ids.insert(next_id.clone());
                state.replay_assistant_message_id = Some(next_id);
            }
            state.replay_assistant_text.push_str(&delta);
            flushed
        };

        if let Some(replay) = flushed {
            self.emit_replay_assistant_message(replay).await;
        }
    }

    async fn handle_replay_tool_call(
        &self,
        params: &Value,
        request: crate::acp::AcpToolCallRequest,
        raw_tool_call_id: String,
        incoming_message_id: Option<String>,
    ) {
        self.flush_replay_user_message().await;
        let workspace_root = self.state.lock().await.workspace_root.clone();
        let tool_type = map_tool_request_type(params, &request.args, &workspace_root).await;

        let flushed = {
            let mut state = self.state.lock().await;
            let should_flush = if let Some(next_id) = incoming_message_id.as_deref() {
                state.replay_assistant_message_id.as_deref() != Some(next_id)
                    && (has_visible_text(&state.replay_assistant_text)
                        || !state.replay_assistant_tool_calls.is_empty())
            } else {
                false
            };

            let flushed = if should_flush {
                Some((
                    std::mem::take(&mut state.replay_assistant_text),
                    std::mem::take(&mut state.replay_assistant_tool_calls),
                    std::mem::take(&mut state.replay_tool_contexts),
                    std::mem::take(&mut state.replay_tool_completion_order),
                ))
            } else {
                None
            };

            if should_flush {
                state.replay_assistant_message_id = None;
            }
            if let Some(next_id) = incoming_message_id {
                state.replay_message_ids.insert(next_id.clone());
                state.replay_assistant_message_id = Some(next_id);
            }

            let tool_call_entry = ToolUseData {
                id: raw_tool_call_id.clone(),
                name: request.tool_name.clone(),
                arguments: request.args.clone(),
            };
            let already_present = state
                .replay_assistant_tool_calls
                .iter()
                .any(|call| call.id == raw_tool_call_id);
            if !already_present {
                state.replay_assistant_tool_calls.push(tool_call_entry);
            }
            state.replay_tool_contexts.insert(
                raw_tool_call_id.clone(),
                KiroReplayToolContext {
                    tool_name: request.tool_name,
                    tool_type,
                    completion: None,
                },
            );
            state.replay_tool_call_ids.insert(raw_tool_call_id);
            flushed
        };

        if let Some(replay) = flushed {
            self.emit_replay_assistant_message(replay).await;
        }
    }

    async fn handle_replay_tool_call_update(
        &self,
        params: &Value,
        raw_tool_call_id: Option<String>,
        message_id: Option<String>,
    ) {
        let fallback_name = {
            let state = self.state.lock().await;
            raw_tool_call_id
                .as_ref()
                .and_then(|id| state.replay_tool_contexts.get(id))
                .map(|ctx| ctx.tool_name.clone())
        };
        let Some(mut completion) = parse_tool_call_completion(params, fallback_name) else {
            return;
        };
        if let Some(raw_tool_call_id) = raw_tool_call_id {
            completion.tool_call_id = raw_tool_call_id;
        }

        let replay_context = {
            let state = self.state.lock().await;
            state
                .replay_tool_contexts
                .get(&completion.tool_call_id)
                .map(|ctx| KiroToolContext {
                    tool_name: ctx.tool_name.clone(),
                    tool_type: ctx.tool_type.clone(),
                    request_emitted: false,
                    pending_completion: None,
                })
        };
        let tool_result = map_tool_completion_result(&completion, replay_context.as_ref());

        let mut state = self.state.lock().await;
        if let Some(message_id) = message_id {
            state.replay_message_ids.insert(message_id);
        }
        let context = state
            .replay_tool_contexts
            .entry(completion.tool_call_id.clone())
            .or_insert_with(|| KiroReplayToolContext {
                tool_name: completion.tool_name.clone(),
                tool_type: ToolRequestType::Other { args: json!({}) },
                completion: None,
            });
        if completion.tool_name == "tool" {
            completion.tool_name = context.tool_name.clone();
        } else {
            context.tool_name = completion.tool_name.clone();
        }
        context.completion = Some(PendingToolCompletion {
            tool_name: completion.tool_name.clone(),
            tool_result,
            success: completion.success,
            error: completion.error.clone(),
        });
        if !state
            .replay_tool_completion_order
            .iter()
            .any(|id| id == &completion.tool_call_id)
        {
            state
                .replay_tool_completion_order
                .push(completion.tool_call_id.clone());
        }
        state.replay_tool_call_ids.insert(completion.tool_call_id);
    }

    async fn emit_replay_user_message(&self, text: String) {
        let content = text.trim().to_string();
        if content.is_empty() {
            return;
        }

        self.emit_event(ChatEvent::MessageAdded(ChatMessage {
            timestamp: unix_now_ms(),
            sender: MessageSender::User,
            content,
            reasoning: None,
            tool_calls: Vec::new(),
            model_info: None,
            token_usage: None,
            context_breakdown: None,
            images: Some(Vec::new()),
        }));
    }

    async fn flush_replay_user_message(&self) {
        let replay = {
            let mut state = self.state.lock().await;
            state.replay_user_message_id = None;
            if has_visible_text(&state.replay_user_text) {
                Some(std::mem::take(&mut state.replay_user_text))
            } else {
                state.replay_user_text.clear();
                None
            }
        };

        if let Some(replay) = replay {
            self.emit_replay_user_message(replay).await;
        }
    }

    async fn emit_replay_assistant_message(
        &self,
        replay: (
            String,
            Vec<ToolUseData>,
            HashMap<String, KiroReplayToolContext>,
            Vec<String>,
        ),
    ) {
        let (text, tool_calls, tool_contexts, completion_order) = replay;
        let cleaned_text = strip_ansi_and_controls(&text);
        if !has_renderable_stream_text(&cleaned_text) && tool_calls.is_empty() {
            return;
        }

        let model = {
            self.state
                .lock()
                .await
                .model
                .clone()
                .unwrap_or_else(|| "kiro".to_string())
        };

        self.emit_event(ChatEvent::MessageAdded(ChatMessage {
            timestamp: unix_now_ms(),
            sender: MessageSender::Assistant {
                agent: KIRO_AGENT_NAME.to_string(),
            },
            content: cleaned_text,
            reasoning: None,
            tool_calls: tool_calls.clone(),
            model_info: Some(ModelInfo { model }),
            token_usage: None,
            context_breakdown: None,
            images: Some(Vec::new()),
        }));

        for tool_call in &tool_calls {
            let tool_call_id = tool_call.id.clone();
            let Some(context) = tool_contexts.get(&tool_call_id) else {
                continue;
            };
            self.emit_event(ChatEvent::ToolRequest(ToolRequest {
                tool_call_id,
                tool_name: context.tool_name.clone(),
                tool_type: context.tool_type.clone(),
            }));
        }

        for tool_call_id in completion_order {
            let Some(context) = tool_contexts.get(&tool_call_id) else {
                continue;
            };
            let Some(completion) = context.completion.clone() else {
                continue;
            };
            self.emit_event(ChatEvent::ToolExecutionCompleted(
                ToolExecutionCompletedData {
                    tool_call_id,
                    tool_name: completion.tool_name,
                    tool_result: completion.tool_result,
                    success: completion.success,
                    error: completion.error,
                },
            ));
        }
    }

    async fn flush_replay_assistant_message(&self) {
        let replay = {
            let mut state = self.state.lock().await;
            let has_content = has_visible_text(&state.replay_assistant_text)
                || !state.replay_assistant_tool_calls.is_empty();
            state.replay_assistant_message_id = None;
            if has_content {
                Some((
                    std::mem::take(&mut state.replay_assistant_text),
                    std::mem::take(&mut state.replay_assistant_tool_calls),
                    std::mem::take(&mut state.replay_tool_contexts),
                    std::mem::take(&mut state.replay_tool_completion_order),
                ))
            } else {
                state.replay_assistant_text.clear();
                state.replay_assistant_tool_calls.clear();
                state.replay_tool_contexts.clear();
                state.replay_tool_completion_order.clear();
                None
            }
        };

        if let Some(replay) = replay {
            self.emit_replay_assistant_message(replay).await;
        }
    }

    async fn flush_replay_history(&self) {
        self.flush_replay_user_message().await;
        self.flush_replay_assistant_message().await;
    }

    fn handle_plan_update(&self, params: &Value) {
        let title = params
            .get("title")
            .or_else(|| params.get("summary"))
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .unwrap_or("Plan")
            .to_string();

        let entries = params
            .get("entries")
            .or_else(|| params.get("tasks"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        let tasks = entries
            .iter()
            .enumerate()
            .map(|(index, step)| {
                let description = step
                    .get("title")
                    .or_else(|| step.get("description"))
                    .and_then(Value::as_str)
                    .unwrap_or("step");
                let status = step
                    .get("status")
                    .and_then(Value::as_str)
                    .map(map_plan_status)
                    .unwrap_or("pending");

                Task {
                    id: index as u64 + 1,
                    description: description.to_string(),
                    status: match status {
                        "in_progress" => TaskStatus::InProgress,
                        "completed" => TaskStatus::Completed,
                        "failed" => TaskStatus::Failed,
                        _ => TaskStatus::Pending,
                    },
                }
            })
            .collect::<Vec<_>>();

        self.emit_event(ChatEvent::TaskUpdate(TaskList { title, tasks }));
    }

    async fn handle_error_notification(&self, params: &Value) {
        let message = params
            .get("message")
            .or_else(|| params.get("error").and_then(|v| v.get("message")))
            .and_then(Value::as_str)
            .unwrap_or("Kiro error")
            .to_string();

        self.clear_active_stream().await;
        self.emit_event(ChatEvent::TypingStatusChanged(false));
        self.emit_event(ChatEvent::Error(message));
    }

    async fn finalize_active_stream_if_any(&self, usage: Option<Value>, end_typing: bool) {
        let active = {
            let mut state = self.state.lock().await;
            state.active_message_id.take().map(|message_id| {
                (
                    message_id,
                    std::mem::take(&mut state.active_stream_text),
                    std::mem::take(&mut state.active_stream_tool_calls),
                )
            })
        };

        if let Some((message_id, text, tool_calls)) = active {
            self.emit_stream_end(message_id, text, usage, tool_calls, end_typing)
                .await;
        } else if end_typing {
            self.emit_event(ChatEvent::TypingStatusChanged(false));
        }
    }

    async fn clear_active_stream(&self) {
        let mut state = self.state.lock().await;
        state.active_message_id = None;
        state.active_stream_text.clear();
        state.active_stream_tool_calls.clear();
        state.active_tool_contexts.clear();
        state.tool_call_aliases.clear();
    }

    async fn emit_stream_end(
        &self,
        _message_id: String,
        text: String,
        token_usage: Option<Value>,
        tool_calls: Vec<ToolUseData>,
        end_typing: bool,
    ) {
        let cleaned_text = strip_ansi_and_controls(&text);
        if !has_renderable_stream_text(&cleaned_text) && tool_calls.is_empty() {
            if end_typing {
                self.emit_event(ChatEvent::TypingStatusChanged(false));
            }
            return;
        }

        let model = {
            self.state
                .lock()
                .await
                .model
                .clone()
                .unwrap_or_else(|| "kiro".to_string())
        };
        let normalized_usage = normalize_token_usage(token_usage.as_ref());
        let context_breakdown = normalized_usage
            .as_ref()
            .map(estimate_context_breakdown_from_usage)
            .and_then(context_breakdown_from_value);
        let tool_calls_for_events = tool_calls.clone();

        self.emit_event(ChatEvent::StreamEnd(StreamEndData {
            message: ChatMessage {
                timestamp: unix_now_ms(),
                sender: MessageSender::Assistant {
                    agent: KIRO_AGENT_NAME.to_string(),
                },
                content: cleaned_text,
                reasoning: None,
                tool_calls,
                model_info: Some(ModelInfo { model }),
                token_usage: normalized_usage.as_ref().and_then(token_usage_from_value),
                context_breakdown,
                images: Some(Vec::new()),
            },
        }));
        self.flush_tool_events_after_stream_end(&tool_calls_for_events)
            .await;
        if end_typing {
            self.emit_event(ChatEvent::TypingStatusChanged(false));
        }
    }

    async fn flush_tool_events_after_stream_end(&self, tool_calls: &[ToolUseData]) {
        let mut completions_to_emit: Vec<(
            String,
            String,
            ToolExecutionResult,
            bool,
            Option<String>,
        )> = Vec::new();
        let mut requests_to_emit: Vec<(String, String, ToolRequestType)> = Vec::new();

        {
            let mut state = self.state.lock().await;
            for tool_call in tool_calls {
                let tool_call_id = tool_call.id.clone();

                if let Some(context) = state.active_tool_contexts.get_mut(&tool_call_id) {
                    if !context.request_emitted {
                        requests_to_emit.push((
                            tool_call_id.clone(),
                            context.tool_name.clone(),
                            context.tool_type.clone(),
                        ));
                        context.request_emitted = true;
                    }
                    if let Some(completion) = context.pending_completion.take() {
                        completions_to_emit.push((
                            tool_call_id.clone(),
                            completion.tool_name,
                            completion.tool_result,
                            completion.success,
                            completion.error,
                        ));
                    }
                }
            }

            for (tool_call_id, _, _, _, _) in &completions_to_emit {
                state.active_tool_contexts.remove(tool_call_id);
                remove_tool_call_aliases(&mut state.tool_call_aliases, tool_call_id, None, None);
            }
        }

        for (tool_call_id, tool_name, tool_type) in requests_to_emit {
            self.emit_event(ChatEvent::ToolRequest(ToolRequest {
                tool_call_id,
                tool_name,
                tool_type,
            }));
        }

        for (tool_call_id, tool_name, tool_result, success, error) in completions_to_emit {
            self.emit_event(ChatEvent::ToolExecutionCompleted(
                ToolExecutionCompletedData {
                    tool_call_id,
                    tool_name,
                    tool_result,
                    success,
                    error,
                },
            ));
        }
    }

    fn emit_user_message_added(&self, content: &str, images: Option<&[ImageAttachment]>) {
        self.emit_event(ChatEvent::MessageAdded(ChatMessage {
            timestamp: unix_now_ms(),
            sender: MessageSender::User,
            content: content.to_string(),
            reasoning: None,
            tool_calls: Vec::new(),
            model_info: None,
            token_usage: None,
            context_breakdown: None,
            images: Some(image_data_from_attachments(images)),
        }));
    }

    fn emit_event(&self, event: ChatEvent) {
        if let Err(error) = self.event_tx.send(event) {
            tracing::error!("Kiro event channel closed: {error}");
        }
    }
}

fn resolve_local_kiro_sessions_dir() -> Result<std::path::PathBuf, String> {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map_err(|_| "Could not determine home directory for Kiro sessions".to_string())?;
    Ok(std::path::PathBuf::from(home)
        .join(".kiro")
        .join("sessions")
        .join("cli"))
}

struct KiroSessionRoots {
    session_cwd: String,
    scope_root: String,
}

async fn resolve_kiro_session_roots(
    workspace_roots: &[String],
    transport: &BackendTransport,
    ephemeral: bool,
) -> Result<KiroSessionRoots, String> {
    let _ = transport;

    let scope_root = pick_workspace_root(workspace_roots)?;
    let session_cwd = if ephemeral {
        let dir = PathBuf::from(&scope_root)
            .join(".tyde")
            .join("kiro-ephemeral");
        tokio::fs::create_dir_all(&dir).await.map_err(|err| {
            format!(
                "Failed to create Kiro ephemeral directory '{}': {err}",
                dir.display()
            )
        })?;
        dir.to_string_lossy().to_string()
    } else {
        scope_root.clone()
    };

    Ok(KiroSessionRoots {
        session_cwd,
        scope_root,
    })
}

fn strip_ansi_and_controls(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            if matches!(chars.peek(), Some('[')) {
                let _ = chars.next();
                for next in chars.by_ref() {
                    if ('@'..='~').contains(&next) {
                        break;
                    }
                }
            }
            continue;
        }
        if matches!(ch, '\u{200B}' | '\u{200C}' | '\u{200D}' | '\u{FEFF}') {
            continue;
        }
        if ch.is_control() && !matches!(ch, '\n' | '\r' | '\t') {
            continue;
        }
        output.push(ch);
    }
    output
}

fn has_visible_text(input: &str) -> bool {
    input.chars().any(|ch| !ch.is_whitespace())
}

fn normalize_tool_call_id_fragment(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        "tool".to_string()
    } else {
        trimmed.to_string()
    }
}

fn tool_alias_raw_key(raw_tool_call_id: &str) -> String {
    format!("raw:{}", normalize_tool_call_id_fragment(raw_tool_call_id))
}

fn tool_alias_message_key(message_id: &str, raw_tool_call_id: &str) -> String {
    format!(
        "msg:{}:{}",
        message_id.trim(),
        normalize_tool_call_id_fragment(raw_tool_call_id)
    )
}

fn build_canonical_tool_call_id(
    _state: &mut KiroState,
    _message_id: &str,
    raw_tool_call_id: &str,
) -> String {
    normalize_tool_call_id_fragment(raw_tool_call_id)
}

fn resolve_tool_call_id_alias(
    state: &KiroState,
    raw_tool_call_id: Option<&str>,
    _message_id: Option<&str>,
) -> Option<String> {
    let raw_tool_call_id = raw_tool_call_id.map(normalize_tool_call_id_fragment)?;

    if state.active_tool_contexts.contains_key(&raw_tool_call_id) {
        return Some(raw_tool_call_id);
    }

    let raw_key = tool_alias_raw_key(&raw_tool_call_id);
    state
        .tool_call_aliases
        .get(&raw_key)
        .cloned()
        .or(Some(raw_tool_call_id))
}

fn remove_tool_call_aliases(
    aliases: &mut HashMap<String, String>,
    canonical_tool_call_id: &str,
    raw_tool_call_id: Option<&str>,
    message_id: Option<&str>,
) {
    if let Some(raw_id) = raw_tool_call_id {
        aliases.remove(&tool_alias_raw_key(raw_id));
        if let Some(message_id) = message_id {
            aliases.remove(&tool_alias_message_key(message_id, raw_id));
        }
    }
    aliases.retain(|_, mapped| mapped != canonical_tool_call_id);
}

fn is_suppressed_history_message(state: &KiroState, message_id: Option<&str>) -> bool {
    let Some(message_id) = message_id.map(str::trim).filter(|value| !value.is_empty()) else {
        return false;
    };
    state.replay_message_ids.contains(message_id)
}

fn is_suppressed_history_tool_event(
    state: &KiroState,
    tool_call_id: Option<&str>,
    message_id: Option<&str>,
) -> bool {
    if let Some(message_id) = message_id.map(str::trim).filter(|value| !value.is_empty()) {
        if state.replay_message_ids.contains(message_id) {
            return true;
        }
    }
    let Some(tool_call_id) = tool_call_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return false;
    };
    state.replay_tool_call_ids.contains(tool_call_id)
}

fn has_renderable_stream_text(input: &str) -> bool {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return false;
    }
    !trimmed.chars().all(is_stream_artifact_char)
}

fn is_stream_artifact_char(ch: char) -> bool {
    matches!(
        ch,
        '\u{2500}'..='\u{259F}' | '\u{25A0}' | '\u{25AA}' | '\u{25AB}' | '\u{FFFD}' | '|'
    )
}

/// Maps Kiro ACP tool_call params to Tyde's internal tool type representation.
/// Uses the ACP `kind` field directly: "execute" → RunCommand, "edit" → ModifyFile, "read" → ReadFiles.
async fn map_tool_request_type(
    params: &Value,
    args: &Value,
    workspace_root: &str,
) -> ToolRequestType {
    let acp_kind = params.get("kind").and_then(Value::as_str).unwrap_or("");

    match acp_kind {
        "execute" => {
            let command = args
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let working_directory = args
                .get("working_dir")
                .and_then(Value::as_str)
                .unwrap_or(workspace_root)
                .to_string();
            ToolRequestType::RunCommand {
                command,
                working_directory,
            }
        }
        "edit" => {
            let file_path = args
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let mut before = args
                .get("oldStr")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let after = args
                .get("newStr")
                .or_else(|| args.get("file_text"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();

            let resolved_file_path = resolve_tool_file_path(&file_path, workspace_root);
            if before.is_empty()
                && !resolved_file_path.is_empty()
                && Path::new(&resolved_file_path).exists()
            {
                if let Ok(contents) = tokio::fs::read_to_string(&resolved_file_path).await {
                    before = contents;
                }
            }

            ToolRequestType::ModifyFile {
                file_path,
                before,
                after,
            }
        }
        "read" => {
            let mut file_paths = Vec::new();
            if let Some(ops) = args.get("ops").and_then(Value::as_array) {
                for op in ops {
                    if let Some(path) = op.get("path").and_then(Value::as_str) {
                        file_paths.push(path.to_string());
                    }
                }
            }
            ToolRequestType::ReadFiles { file_paths }
        }
        _ => ToolRequestType::Other { args: args.clone() },
    }
}

fn extract_kiro_message_id(value: &Value) -> Option<String> {
    extract_message_id(value).or_else(|| {
        extract_first_string_deep(
            value,
            &[
                "messageId",
                "message_id",
                "assistantMessageId",
                "assistant_message_id",
                "itemId",
                "item_id",
                "responseMessageId",
                "response_message_id",
            ],
        )
    })
}

fn extract_kiro_tool_call_id(value: &Value) -> Option<String> {
    extract_tool_call_id(value).or_else(|| {
        extract_first_string_deep(value, &["toolCallId", "tool_call_id", "callId", "call_id"])
    })
}

/// Maps a Kiro ACP tool completion to Tyde's internal result representation.
/// Uses the ACP `kind` field: "execute" → RunCommand, "edit" → ModifyFile, "read" → ReadFiles.
/// The `rawOutput` for execute completions is: `{"items": [{"Json": {"exit_status": "exit status: N", "stdout": "...", "stderr": "..."}}]}`
/// The `rawOutput` for read completions is: `{"items": [{"Text": "..."}]}`
/// The `rawOutput` for edit completions is: `{"items": [{"Text": ""}]}`
fn map_tool_completion_result(
    completion: &crate::acp::AcpToolCallCompletion,
    context: Option<&KiroToolContext>,
) -> ToolExecutionResult {
    if !completion.success {
        let short_message = completion
            .error
            .clone()
            .unwrap_or_else(|| format!("{} failed", completion.tool_name));
        let detailed_message = serde_json::to_string_pretty(&completion.tool_result)
            .unwrap_or_else(|_| completion.tool_result.to_string());
        return ToolExecutionResult::Error {
            short_message,
            detailed_message,
        };
    }

    match completion.kind.as_str() {
        "execute" => {
            let json_obj = extract_first_item_json(&completion.tool_result);
            let exit_code = json_obj
                .and_then(|obj| obj.get("exit_status").and_then(Value::as_str))
                .and_then(|s| s.rsplit(':').next())
                .and_then(|n| n.trim().parse::<i64>().ok())
                .and_then(|code| i32::try_from(code).ok())
                .unwrap_or(0);
            let stdout = json_obj
                .and_then(|obj| obj.get("stdout").and_then(Value::as_str))
                .unwrap_or("")
                .to_string();
            let stderr = json_obj
                .and_then(|obj| obj.get("stderr").and_then(Value::as_str))
                .unwrap_or("")
                .to_string();
            ToolExecutionResult::RunCommand {
                exit_code,
                stdout,
                stderr,
            }
        }
        "edit" => {
            let before = context
                .and_then(|ctx| match &ctx.tool_type {
                    ToolRequestType::ModifyFile { before, .. } => Some(before.as_str()),
                    _ => None,
                })
                .unwrap_or_default();
            let after = context
                .and_then(|ctx| match &ctx.tool_type {
                    ToolRequestType::ModifyFile { after, .. } => Some(after.as_str()),
                    _ => None,
                })
                .unwrap_or_default();
            let (lines_added, lines_removed) = estimate_line_diff_counts(before, after);
            ToolExecutionResult::ModifyFile {
                lines_added,
                lines_removed,
            }
        }
        "read" => {
            let file_paths = context.and_then(|ctx| match &ctx.tool_type {
                ToolRequestType::ReadFiles { file_paths } => Some(file_paths.as_slice()),
                _ => None,
            });
            let text_len = extract_first_item_text(&completion.tool_result)
                .map(|t| t.len() as u64)
                .unwrap_or(0);
            let files: Vec<FileInfo> = file_paths
                .into_iter()
                .flatten()
                .map(|path| FileInfo {
                    path: path.clone(),
                    bytes: text_len,
                })
                .collect();
            ToolExecutionResult::ReadFiles { files }
        }
        _ => ToolExecutionResult::Other {
            result: completion.tool_result.clone(),
        },
    }
}

/// Extracts the first `{"Json": {...}}` item from `{"items": [{"Json": {...}}]}`.
fn extract_first_item_json(value: &Value) -> Option<&Value> {
    value
        .get("items")
        .and_then(Value::as_array)
        .and_then(|items| items.first())
        .and_then(|item| item.get("Json"))
}

/// Extracts the first `{"Text": "..."}` item from `{"items": [{"Text": "..."}]}`.
fn extract_first_item_text(value: &Value) -> Option<&str> {
    value
        .get("items")
        .and_then(Value::as_array)
        .and_then(|items| items.first())
        .and_then(|item| item.get("Text"))
        .and_then(Value::as_str)
}

fn estimate_line_diff_counts(before: &str, after: &str) -> (u64, u64) {
    if before == after {
        return (0, 0);
    }
    let before_lines = before.lines().count() as i64;
    let after_lines = after.lines().count() as i64;
    if after_lines >= before_lines {
        ((after_lines - before_lines) as u64, 0)
    } else {
        (0, (before_lines - after_lines) as u64)
    }
}

fn extract_first_string(value: &Value, keys: &[&str]) -> Option<String> {
    for key in keys {
        let Some(raw) = value.get(*key) else {
            continue;
        };
        if let Some(text) = raw.as_str() {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

fn extract_first_string_deep(value: &Value, keys: &[&str]) -> Option<String> {
    extract_first_string_recursive(value, keys, 0, 5)
}

fn extract_first_string_recursive(
    value: &Value,
    keys: &[&str],
    depth: usize,
    max_depth: usize,
) -> Option<String> {
    if depth > max_depth {
        return None;
    }
    if let Some(found) = extract_first_string(value, keys) {
        return Some(found);
    }

    match value {
        Value::Object(map) => {
            for child in map.values() {
                if let Some(parsed) = parse_json_value_from_string(child) {
                    if let Some(found) =
                        extract_first_string_recursive(&parsed, keys, depth + 1, max_depth)
                    {
                        return Some(found);
                    }
                }
                if let Some(found) =
                    extract_first_string_recursive(child, keys, depth + 1, max_depth)
                {
                    return Some(found);
                }
            }
            None
        }
        Value::Array(items) => {
            for child in items {
                if let Some(parsed) = parse_json_value_from_string(child) {
                    if let Some(found) =
                        extract_first_string_recursive(&parsed, keys, depth + 1, max_depth)
                    {
                        return Some(found);
                    }
                }
                if let Some(found) =
                    extract_first_string_recursive(child, keys, depth + 1, max_depth)
                {
                    return Some(found);
                }
            }
            None
        }
        _ => {
            if let Some(parsed) = parse_json_value_from_string(value) {
                return extract_first_string_recursive(&parsed, keys, depth + 1, max_depth);
            }
            None
        }
    }
}

fn parse_json_value_from_string(value: &Value) -> Option<Value> {
    let raw = value.as_str()?.trim();
    if !(raw.starts_with('{') || raw.starts_with('[')) {
        return None;
    }
    serde_json::from_str::<Value>(raw).ok()
}

fn resolve_tool_file_path(file_path: &str, workspace_root: &str) -> String {
    let trimmed = file_path.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let path = Path::new(trimmed);
    if path.is_absolute() {
        return trimmed.to_string();
    }
    PathBuf::from(workspace_root)
        .join(path)
        .to_string_lossy()
        .to_string()
}

const KIRO_ESTIMATED_BYTES_PER_TOKEN: u64 = 4;
const KIRO_ESTIMATED_CONTEXT_WINDOW: u64 = 200_000;
const KIRO_MIN_SYSTEM_PROMPT_BYTES: u64 = 1_024;

fn image_data_from_attachments(images: Option<&[ImageAttachment]>) -> Vec<ImageData> {
    images
        .unwrap_or(&[])
        .iter()
        .map(|image| ImageData {
            media_type: image.media_type.clone(),
            data: image.data.clone(),
        })
        .collect()
}

fn normalize_token_usage(raw: Option<&Value>) -> Option<Value> {
    let raw = raw?;
    let source = raw
        .get("last")
        .or_else(|| raw.get("usage"))
        .or_else(|| raw.get("tokenUsage"))
        .or_else(|| raw.get("token_usage"))
        .filter(|value| value.is_object())
        .unwrap_or(raw);

    let cached_prompt_tokens = usage_u64(
        source,
        &[
            "cached_prompt_tokens",
            "cachedInputTokens",
            "cache_read_input_tokens",
            "cacheReadInputTokens",
        ],
    )
    .unwrap_or(0);
    let cache_creation_input_tokens = usage_u64(
        source,
        &[
            "cache_creation_input_tokens",
            "cacheCreationInputTokens",
            "cacheWriteInputTokens",
            "cache_write_input_tokens",
        ],
    )
    .unwrap_or(0);

    let has_total_prompt_input = source.get("inputTokens").is_some()
        || source.get("promptTokens").is_some()
        || source.get("prompt_tokens").is_some();
    let raw_prompt_input = usage_u64(
        source,
        &[
            "inputTokens",
            "promptTokens",
            "prompt_tokens",
            "input_tokens_total",
            "inputTokenCount",
            "promptTokenCount",
        ],
    )
    .unwrap_or(0);
    let input_tokens = if has_total_prompt_input {
        raw_prompt_input
            .saturating_sub(cached_prompt_tokens)
            .saturating_sub(cache_creation_input_tokens)
    } else {
        usage_u64(source, &["input_tokens", "inputTokens"]).unwrap_or(raw_prompt_input)
    };

    let output_tokens = usage_u64(
        source,
        &[
            "output_tokens",
            "outputTokens",
            "completion_tokens",
            "completionTokens",
            "outputTokenCount",
            "completionTokenCount",
        ],
    )
    .unwrap_or(0);
    let reasoning_tokens = usage_u64(
        source,
        &[
            "reasoning_tokens",
            "reasoningTokens",
            "reasoningOutputTokens",
            "reasoningTokenCount",
        ],
    )
    .unwrap_or(0);
    let total_tokens = usage_u64(source, &["total_tokens", "totalTokens", "totalTokenCount"])
        .unwrap_or(input_tokens.saturating_add(output_tokens));
    let context_window = usage_u64(
        raw,
        &[
            "context_window",
            "contextWindow",
            "maxInputTokens",
            "max_input_tokens",
            "maxTokens",
            "max_tokens",
            "contextLength",
        ],
    )
    .or_else(|| {
        usage_u64(
            source,
            &[
                "context_window",
                "contextWindow",
                "maxInputTokens",
                "max_input_tokens",
                "maxTokens",
                "max_tokens",
                "contextLength",
            ],
        )
    });

    if input_tokens == 0
        && output_tokens == 0
        && total_tokens == 0
        && cached_prompt_tokens == 0
        && cache_creation_input_tokens == 0
        && reasoning_tokens == 0
    {
        return None;
    }

    Some(json!({
        "input_tokens": input_tokens,
        "output_tokens": output_tokens,
        "total_tokens": total_tokens,
        "cached_prompt_tokens": cached_prompt_tokens,
        "cache_creation_input_tokens": cache_creation_input_tokens,
        "reasoning_tokens": reasoning_tokens,
        "context_window": context_window,
    }))
}

fn token_usage_from_value(value: &Value) -> Option<TokenUsage> {
    Some(TokenUsage {
        input_tokens: usage_u64(value, &["input_tokens"]).unwrap_or(0),
        output_tokens: usage_u64(value, &["output_tokens"]).unwrap_or(0),
        total_tokens: usage_u64(value, &["total_tokens"]).unwrap_or(0),
        cached_prompt_tokens: Some(usage_u64(value, &["cached_prompt_tokens"]).unwrap_or(0)),
        cache_creation_input_tokens: Some(
            usage_u64(value, &["cache_creation_input_tokens"]).unwrap_or(0),
        ),
        reasoning_tokens: Some(usage_u64(value, &["reasoning_tokens"]).unwrap_or(0)),
    })
}

fn usage_u64(value: &Value, keys: &[&str]) -> Option<u64> {
    for key in keys {
        let Some(raw) = value.get(*key) else {
            continue;
        };
        if let Some(number) = raw.as_u64() {
            return Some(number);
        }
        if let Some(number) = raw.as_i64() {
            if number >= 0 {
                return Some(number as u64);
            }
        }
        if let Some(text) = raw.as_str() {
            if let Ok(parsed) = text.trim().parse::<u64>() {
                return Some(parsed);
            }
        }
    }
    None
}

fn estimate_context_breakdown_from_usage(token_usage: &Value) -> Value {
    let base_input_tokens = token_usage
        .get("input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cached_prompt_tokens = token_usage
        .get("cached_prompt_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cache_creation_input_tokens = token_usage
        .get("cache_creation_input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let reasoning_tokens = token_usage
        .get("reasoning_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);

    let input_tokens = base_input_tokens
        .saturating_add(cached_prompt_tokens)
        .saturating_add(cache_creation_input_tokens);
    let context_window = token_usage
        .get("context_window")
        .and_then(Value::as_u64)
        .filter(|window| *window > 0)
        .unwrap_or_else(|| std::cmp::max(KIRO_ESTIMATED_CONTEXT_WINDOW, input_tokens.max(1)));

    let total_prompt_bytes = input_tokens.saturating_mul(KIRO_ESTIMATED_BYTES_PER_TOKEN);
    let system_prompt_bytes = if total_prompt_bytes == 0 {
        0
    } else {
        std::cmp::min(
            total_prompt_bytes,
            std::cmp::max(KIRO_MIN_SYSTEM_PROMPT_BYTES, total_prompt_bytes / 10),
        )
    };

    let mut remaining = total_prompt_bytes.saturating_sub(system_prompt_bytes);
    let reasoning_bytes = std::cmp::min(
        remaining,
        reasoning_tokens.saturating_mul(KIRO_ESTIMATED_BYTES_PER_TOKEN),
    );
    remaining = remaining.saturating_sub(reasoning_bytes);

    let tool_io_bytes = std::cmp::min(remaining, total_prompt_bytes / 20);
    remaining = remaining.saturating_sub(tool_io_bytes);
    let history_bytes = remaining;

    json!({
        "system_prompt_bytes": system_prompt_bytes,
        "tool_io_bytes": tool_io_bytes,
        "history_bytes": history_bytes,
        "reasoning_bytes": reasoning_bytes,
        "context_injection_bytes": 0,
        "input_tokens": input_tokens,
        "context_window": context_window,
    })
}

fn context_breakdown_from_value(value: Value) -> Option<ContextBreakdown> {
    Some(ContextBreakdown {
        system_prompt_bytes: value
            .get("system_prompt_bytes")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        tool_io_bytes: value
            .get("tool_io_bytes")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        conversation_history_bytes: value
            .get("conversation_history_bytes")
            .or_else(|| value.get("history_bytes"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
        reasoning_bytes: value
            .get("reasoning_bytes")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        context_injection_bytes: value
            .get("context_injection_bytes")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        input_tokens: value
            .get("input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        context_window: value
            .get("context_window")
            .and_then(Value::as_u64)
            .unwrap_or(0),
    })
}

fn extract_current_model(value: &Value) -> Option<String> {
    value
        .get("model")
        .or_else(|| value.get("currentModelId"))
        .or_else(|| value.get("modelId"))
        .or_else(|| {
            value
                .get("models")
                .and_then(|models| models.get("currentModelId"))
        })
        .and_then(Value::as_str)
        .map(|raw| raw.trim().to_string())
        .filter(|raw| !raw.is_empty())
}

fn extract_current_mode(value: &Value) -> Option<String> {
    value
        .get("mode")
        .or_else(|| value.get("currentModeId"))
        .or_else(|| value.get("modeId"))
        .or_else(|| {
            value
                .get("modes")
                .and_then(|modes| modes.get("currentModeId"))
        })
        .and_then(Value::as_str)
        .map(|raw| raw.trim().to_string())
        .filter(|raw| !raw.is_empty())
}

fn extract_known_models(value: &Value) -> Vec<ModelsListEntry> {
    let models = value
        .get("models")
        .and_then(|models| {
            models
                .get("availableModels")
                .or_else(|| models.get("models"))
                .or_else(|| models.get("available"))
        })
        .or_else(|| value.get("availableModels"));

    let raw_models = models
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    raw_models
        .iter()
        .filter_map(|model| {
            let id = model
                .get("id")
                .or_else(|| model.get("modelId"))
                .or_else(|| model.get("name"))
                .and_then(Value::as_str)?;
            let display_name = model
                .get("name")
                .or_else(|| model.get("displayName"))
                .and_then(Value::as_str)
                .unwrap_or(id);
            let is_default = model
                .get("isDefault")
                .or_else(|| model.get("default"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            Some(ModelsListEntry {
                id: id.to_string(),
                display_name: display_name.to_string(),
                is_default,
            })
        })
        .collect()
}

fn normalize_optional_string(value: &Value) -> Option<String> {
    if value.is_null() {
        return None;
    }
    value
        .as_str()
        .map(str::trim)
        .filter(|raw| !raw.is_empty())
        .map(|raw| raw.to_string())
}

fn find_in_path(binary: &str) -> Option<String> {
    let which_cmd = if cfg!(windows) { "where" } else { "which" };
    let output = std::process::Command::new(which_cmd)
        .arg(binary)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let path = String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()?
        .trim()
        .to_string();
    if path.is_empty() {
        None
    } else {
        Some(path)
    }
}

/// Toolbox-style wrappers often symlink only the primary binary (kiro-cli)
/// without creating links for companion binaries (kiro-cli-chat). Resolve
/// the real install directory by following symlinks, then look for the
/// companion as a sibling.
fn resolve_sibling_binary(known_binary: &str, sibling_name: &str) -> Option<String> {
    let known_path = find_in_path(known_binary)?;
    let real_path = std::fs::canonicalize(&known_path).ok()?;
    let dir = real_path.parent()?;
    let sibling = dir.join(sibling_name);
    if sibling.exists() {
        Some(sibling.to_string_lossy().to_string())
    } else {
        None
    }
}

fn resolve_kiro_chat_binary() -> String {
    if let Some(path) = find_in_path("kiro-cli-chat") {
        return path;
    }
    if let Some(path) = resolve_sibling_binary("kiro-cli", "kiro-cli-chat") {
        return path;
    }
    "kiro-cli-chat".to_string()
}

fn pick_workspace_root(workspace_roots: &[String]) -> Result<String, String> {
    workspace_roots
        .iter()
        .find(|root| !root.trim().is_empty() && !root.starts_with("ssh://"))
        .cloned()
        .ok_or("Kiro backend requires at least one local workspace root".to_string())
}

fn parse_iso8601_to_unix_ms(s: &str) -> Option<u64> {
    let utc = s.trim().strip_suffix('Z').unwrap_or(s.trim());
    let (date, time) = utc.split_once('T')?;
    let mut dp = date.splitn(3, '-');
    let y: u64 = dp.next()?.parse().ok()?;
    let m: u64 = dp.next()?.parse().ok()?;
    let d: u64 = dp.next()?.parse().ok()?;
    let (hms, _frac) = time.split_once('.').unwrap_or((time, ""));
    let mut tp = hms.splitn(3, ':');
    let h: u64 = tp.next()?.parse().ok()?;
    let min: u64 = tp.next()?.parse().ok()?;
    let sec: u64 = tp.next().and_then(|v| v.parse().ok()).unwrap_or(0);
    let month_days: [u64; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut days: u64 = 0;
    for yr in 1970..y {
        days += if yr.is_multiple_of(4) && (!yr.is_multiple_of(100) || yr.is_multiple_of(400)) {
            366
        } else {
            365
        };
    }
    for mo in 1..m {
        days += month_days.get((mo - 1) as usize).copied().unwrap_or(30);
        if mo == 2 && y.is_multiple_of(4) && (!y.is_multiple_of(100) || y.is_multiple_of(400)) {
            days += 1;
        }
    }
    days += d.saturating_sub(1);
    Some((days * 86400 + h * 3600 + min * 60 + sec) * 1000)
}

fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0))
        .as_millis() as u64
}

#[cfg(unix)]
fn is_pid_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(windows)]
fn is_pid_alive(pid: u32) -> bool {
    std::process::Command::new("cmd")
        .args([
            "/C",
            &format!("tasklist /FI \"PID eq {pid}\" /NH | findstr {pid}"),
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

async fn clear_local_kiro_session_lock(session_id: &str) -> Result<(), String> {
    let sessions_dir = resolve_local_kiro_sessions_dir()?;
    let lock_path = sessions_dir.join(format!("{session_id}.lock"));
    if !lock_path.exists() {
        return Ok(());
    }
    let content = match tokio::fs::read_to_string(&lock_path).await {
        Ok(c) => c,
        Err(_) => return Ok(()),
    };
    if let Ok(pid) = content.trim().parse::<u32>() {
        if is_pid_alive(pid) {
            return Ok(());
        }
    }
    tokio::fs::remove_file(&lock_path)
        .await
        .map_err(|err| format!("Failed to remove stale lock {}: {err}", lock_path.display()))?;
    Ok(())
}

async fn clear_kiro_session_lock_for_transport(
    transport: &BackendTransport,
    session_id: &str,
) -> Result<(), String> {
    let _ = transport;
    clear_local_kiro_session_lock(session_id).await
}

async fn delete_local_kiro_session(session_id: &str) -> Result<(), String> {
    let sessions_dir = resolve_local_kiro_sessions_dir()?;
    for ext in &["json", "jsonl", "lock"] {
        let path = sessions_dir.join(format!("{session_id}.{ext}"));
        if path.exists() {
            tokio::fs::remove_file(&path)
                .await
                .map_err(|err| format!("Failed to delete {}: {err}", path.display()))?;
        }
    }
    Ok(())
}

async fn delete_kiro_session_for_transport(
    transport: &BackendTransport,
    session_id: &str,
) -> Result<(), String> {
    let _ = transport;
    delete_local_kiro_session(session_id).await
}

async fn load_local_kiro_sessions() -> Result<Vec<(String, Value)>, String> {
    let dir = resolve_local_kiro_sessions_dir()?;
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut result = Vec::new();
    let mut entries = tokio::fs::read_dir(&dir)
        .await
        .map_err(|e| format!("Failed to read kiro sessions directory: {e:?}"))?;
    while let Some(entry) = entries
        .next_entry()
        .await
        .map_err(|e| format!("Failed to read directory entry: {e:?}"))?
    {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let session_id = match path.file_stem().and_then(|s| s.to_str()) {
            Some(id) if !id.is_empty() => id.to_string(),
            _ => continue,
        };
        let content = match tokio::fs::read_to_string(&path).await {
            Ok(c) => c,
            Err(e) => {
                tracing::debug!(
                    "Skipping unreadable kiro session file {}: {e:?}",
                    path.display()
                );
                continue;
            }
        };
        let metadata: Value = match serde_json::from_str(&content) {
            Ok(v) => v,
            Err(e) => {
                tracing::debug!(
                    "Skipping unparseable kiro session file {}: {e:?}",
                    path.display()
                );
                continue;
            }
        };
        result.push((session_id, metadata));
    }
    Ok(result)
}

async fn load_kiro_sessions_for_transport(
    transport: &BackendTransport,
) -> Result<Vec<(String, Value)>, String> {
    let _ = transport;
    load_local_kiro_sessions().await
}

fn strip_kiro_steering_prefix<'a>(prompt: &'a str, steering_content: Option<&str>) -> &'a str {
    let Some(steering) = steering_content
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return prompt;
    };

    let prompt = prompt.trim_start();
    if !prompt.starts_with(steering) {
        return prompt;
    }

    prompt[steering.len()..].trim_start_matches(['\r', '\n'])
}

fn normalize_kiro_resume_error(err: String) -> String {
    let normalized = err.to_ascii_lowercase();
    if normalized.contains("session is active in another process") {
        return "This Kiro session is already active in another process and cannot be resumed here until that process stops."
            .to_string();
    }
    err
}

fn extract_session_title(metadata: &Value) -> String {
    metadata
        .get("title")
        .or_else(|| {
            metadata
                .get("conversation_metadata")
                .and_then(|cm| cm.get("title"))
        })
        .or_else(|| metadata.get("name"))
        .and_then(Value::as_str)
        .filter(|t| !t.trim().is_empty())
        .unwrap_or("Kiro Session")
        .to_string()
}

fn extract_session_timestamp(metadata: &Value) -> u64 {
    let ts_field = metadata
        .get("updatedAt")
        .or_else(|| metadata.get("updated_at"))
        .or_else(|| metadata.get("createdAt"))
        .or_else(|| metadata.get("created_at"));
    if let Some(s) = ts_field.and_then(Value::as_str) {
        if let Some(ms) = parse_iso8601_to_unix_ms(s) {
            return ms;
        }
    }
    ts_field.and_then(Value::as_u64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use serde_json::Value;
    use std::collections::HashMap;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    use tokio::sync::{mpsc, Mutex};

    // Real Kiro ACP events captured from a live session.

    async fn test_kiro_inner() -> (Arc<KiroInner>, mpsc::UnboundedReceiver<ChatEvent>) {
        let (bridge, _inbound_rx) = AcpBridge::spawn(
            AcpSpawnSpec::new("Test ACP", "cat", &[]),
            BackendTransport::Local,
        )
        .await
        .expect("spawn test ACP bridge");
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let inner = Arc::new(KiroInner {
            bridge,
            event_tx,
            state: Mutex::new(KiroState {
                session_id: "session-test".to_string(),
                workspace_root: "/mock/workspace".to_string(),
                admin_session: false,
                steering_content: None,
                startup_mcp_servers: Vec::new(),
                model: Some("kiro".to_string()),
                mode: None,
                known_models: Vec::new(),
                active_message_id: None,
                active_stream_text: String::new(),
                active_stream_tool_calls: Vec::new(),
                active_tool_contexts: HashMap::new(),
                tool_call_aliases: HashMap::new(),
                cancelled: false,
                replaying_history: false,
                replay_user_message_id: None,
                replay_user_text: String::new(),
                replay_assistant_message_id: None,
                replay_assistant_text: String::new(),
                replay_assistant_tool_calls: Vec::new(),
                replay_tool_contexts: HashMap::new(),
                replay_tool_completion_order: Vec::new(),
                replay_message_ids: HashSet::new(),
                replay_tool_call_ids: HashSet::new(),
            }),
            shutting_down: AtomicBool::new(false),
            transport: BackendTransport::Local,
        });
        (inner, event_rx)
    }

    fn drain_events(rx: &mut mpsc::UnboundedReceiver<ChatEvent>) -> Vec<Value> {
        let mut out = Vec::new();
        while let Ok(event) = rx.try_recv() {
            out.push(serde_json::to_value(event).unwrap_or_default());
        }
        out
    }

    #[tokio::test]
    async fn test_execute_tool_request_maps_to_run_command() {
        let params = json!({"kind":"execute","rawInput":{"__tool_use_purpose":"Run the hello.py script with python3","command":"python3 hello.py","working_dir":"."},"sessionUpdate":"tool_call","title":"Running: python3 hello.py","toolCallId":"tooluse_969cHK9lEAMViobov6gYma"});
        let args = params.get("rawInput").cloned().unwrap();
        let result = map_tool_request_type(&params, &args, "/workspace").await;

        assert_eq!(result["kind"], "RunCommand");
        assert_eq!(result["command"], "python3 hello.py");
        assert_eq!(result["working_directory"], ".");
    }

    #[tokio::test]
    async fn test_edit_tool_request_maps_to_modify_file() {
        let params = json!({"content":[{"newText":"new content","oldText":"old content","path":"hello.py","type":"diff"}],"kind":"edit","locations":[{"line":25,"path":"hello.py"}],"rawInput":{"command":"strReplace","newStr":"new content","oldStr":"old content","path":"hello.py"},"sessionUpdate":"tool_call","title":"Editing hello.py","toolCallId":"tooluse_TovOchWXoPj9HmcMiNlrCl"});
        let args = params.get("rawInput").cloned().unwrap();
        let result = map_tool_request_type(&params, &args, "/workspace").await;

        assert_eq!(result["kind"], "ModifyFile");
        assert_eq!(result["file_path"], "hello.py");
        assert_eq!(result["before"], "old content");
        assert_eq!(result["after"], "new content");
    }

    #[tokio::test]
    async fn test_read_tool_request_maps_to_read_files() {
        let params = json!({"kind":"read","locations":[{"path":"hello.py"}],"rawInput":{"ops":[{"path":"hello.py"}]},"sessionUpdate":"tool_call","title":"Reading hello.py:1","toolCallId":"tooluse_LPjch7ginxZKkwYbJ42qHB"});
        let args = params.get("rawInput").cloned().unwrap();
        let result = map_tool_request_type(&params, &args, "/workspace").await;

        assert_eq!(result["kind"], "ReadFiles");
        assert_eq!(result["file_paths"], json!(["hello.py"]));
    }

    #[test]
    fn test_execute_completion_maps_to_run_command() {
        let completion = crate::acp::AcpToolCallCompletion {
            tool_call_id: "tooluse_JlKHotZOrwGPRT9StkV4hw".to_string(),
            tool_name: "Running: python3 hello.py".to_string(),
            kind: "execute".to_string(),
            success: true,
            tool_result: json!({"items":[{"Json":{"exit_status":"exit status: 0","stderr":"","stdout":"hello world\n"}}]}),
            error: None,
        };
        let result = map_tool_completion_result(&completion, None);

        assert_eq!(result["kind"], "RunCommand");
        assert_eq!(result["exit_code"], 0);
        assert_eq!(result["stdout"], "hello world\n");
        assert_eq!(result["stderr"], "");
    }

    #[test]
    fn test_execute_completion_nonzero_exit() {
        let completion = crate::acp::AcpToolCallCompletion {
            tool_call_id: "tooluse_gI6kXzqrBXCEjGIqUMooRg".to_string(),
            tool_name: "Running: python hello.py".to_string(),
            kind: "execute".to_string(),
            success: true,
            tool_result: json!({"items":[{"Json":{"exit_status":"exit status: 127","stderr":"bash: python: command not found\n","stdout":""}}]}),
            error: None,
        };
        let result = map_tool_completion_result(&completion, None);

        assert_eq!(result["kind"], "RunCommand");
        assert_eq!(result["exit_code"], 127);
        assert_eq!(result["stderr"], "bash: python: command not found\n");
        assert_eq!(result["stdout"], "");
    }

    #[test]
    fn test_edit_completion_maps_to_modify_file() {
        let context = KiroToolContext {
            tool_name: "write".to_string(),
            tool_type: json!({"kind": "ModifyFile", "file_path": "hello.py", "before": "line1\nline2\n", "after": "line1\nline2\nline3\n"}),
            request_emitted: true,
            pending_completion: None,
        };
        let completion = crate::acp::AcpToolCallCompletion {
            tool_call_id: "tooluse_TovOchWXoPj9HmcMiNlrCl".to_string(),
            tool_name: "Editing hello.py".to_string(),
            kind: "edit".to_string(),
            success: true,
            tool_result: json!({"items":[{"Text":""}]}),
            error: None,
        };
        let result = map_tool_completion_result(&completion, Some(&context));

        assert_eq!(result["kind"], "ModifyFile");
        assert_eq!(result["lines_added"], 1);
        assert_eq!(result["lines_removed"], 0);
    }

    #[test]
    fn test_read_completion_maps_to_read_files() {
        let context = KiroToolContext {
            tool_name: "read".to_string(),
            tool_type: json!({"kind": "ReadFiles", "file_paths": ["hello.py"]}),
            request_emitted: true,
            pending_completion: None,
        };
        let completion = crate::acp::AcpToolCallCompletion {
            tool_call_id: "tooluse_LPjch7ginxZKkwYbJ42qHB".to_string(),
            tool_name: "Reading hello.py:1".to_string(),
            kind: "read".to_string(),
            success: true,
            tool_result: json!({"items":[{"Text":"import random\nimport time\n"}]}),
            error: None,
        };
        let result = map_tool_completion_result(&completion, Some(&context));

        assert_eq!(result["kind"], "ReadFiles");
        let files = result["files"].as_array().unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0]["path"], "hello.py");
        assert_eq!(files[0]["bytes"], 26);
    }

    #[tokio::test]
    async fn replay_notifications_restore_cancelled_tool_history_without_live_busy() {
        let (inner, mut event_rx) = test_kiro_inner().await;
        {
            let mut state = inner.state.lock().await;
            state.replaying_history = true;
            state.steering_content =
                Some("# Tyde Steering Guidelines\n\nReply with KIRO_QA_OK only.".to_string());
        }

        inner
            .handle_standard_update(&json!({
                "sessionUpdate": "user_message_chunk",
                "content": {
                    "type": "text",
                    "text": "# Tyde Steering Guidelines\n\nReply with KIRO_QA_OK only.\n\nPlease run a command that sleeps for 30 seconds and then prints done."
                }
            }))
            .await;
        inner
            .handle_standard_update(&json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "tooluse_54hq8DIXLnwyFlcnN8VpVp",
                "title": "shell",
                "kind": "execute",
                "rawInput": {
                    "command": "sleep 30 && echo done",
                    "__tool_use_purpose": "Run sleep 30 then print done"
                }
            }))
            .await;
        inner
            .handle_standard_update(&json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "tooluse_54hq8DIXLnwyFlcnN8VpVp",
                "title": "shell",
                "kind": "execute",
                "status": "failed"
            }))
            .await;
        inner
            .handle_standard_update(&json!({
                "sessionUpdate": "agent_message_chunk",
                "content": {
                    "type": "text",
                    "text": "Tool uses were interrupted, waiting for the next user prompt"
                }
            }))
            .await;
        inner.flush_replay_history().await;

        let events = drain_events(&mut event_rx);
        let kinds = events
            .iter()
            .filter_map(|event| event.get("kind").and_then(Value::as_str))
            .collect::<Vec<_>>();
        assert_eq!(
            kinds,
            vec![
                "MessageAdded",
                "MessageAdded",
                "ToolRequest",
                "ToolExecutionCompleted",
                "MessageAdded",
            ]
        );

        assert_eq!(
            events[0]["data"]["content"].as_str(),
            Some("Please run a command that sleeps for 30 seconds and then prints done.")
        );
        assert_eq!(
            events[1]["data"]["tool_calls"][0]["id"].as_str(),
            Some("tooluse_54hq8DIXLnwyFlcnN8VpVp")
        );
        assert_eq!(
            events[2]["data"]["tool_type"]["kind"].as_str(),
            Some("RunCommand")
        );
        assert_eq!(events[3]["data"]["success"].as_bool(), Some(false));
        assert_eq!(
            events[4]["data"]["content"].as_str(),
            Some("Tool uses were interrupted, waiting for the next user prompt")
        );

        inner.shutdown().await;
    }

    #[tokio::test]
    async fn late_replay_chunk_after_resume_stays_historical() {
        let (inner, mut event_rx) = test_kiro_inner().await;
        inner.flush_replay_history().await;
        {
            let mut state = inner.state.lock().await;
            state.replaying_history = true;
        }

        inner
            .handle_agent_message_chunk(&json!({
                "text": "late replay chunk",
            }))
            .await;
        assert!(drain_events(&mut event_rx).is_empty());

        inner.flush_replay_history().await;
        let events = drain_events(&mut event_rx);
        let kinds = events
            .iter()
            .filter_map(|event| event.get("kind").and_then(Value::as_str))
            .collect::<Vec<_>>();
        assert_eq!(kinds, vec!["MessageAdded"]);
        assert_eq!(
            events[0]["data"]["sender"]["Assistant"]["agent"].as_str(),
            Some("kiro")
        );

        inner.shutdown().await;
    }

    #[tokio::test]
    async fn prepare_for_live_prompt_flushes_replay_history_and_clears_replay_mode() {
        let (inner, mut event_rx) = test_kiro_inner().await;
        {
            let mut state = inner.state.lock().await;
            state.replaying_history = true;
            state.replay_user_text = "restored user".to_string();
        }

        inner.prepare_for_live_prompt().await;

        let events = drain_events(&mut event_rx);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["kind"].as_str(), Some("MessageAdded"));
        assert_eq!(events[0]["data"]["content"].as_str(), Some("restored user"));
        assert!(!inner.state.lock().await.replaying_history);

        inner.shutdown().await;
    }

    #[tokio::test]
    async fn handle_agent_message_chunk_ignores_replayed_message_ids() {
        let (inner, mut event_rx) = test_kiro_inner().await;
        {
            let mut state = inner.state.lock().await;
            state.replay_message_ids.insert("history-msg-1".to_string());
        }

        inner
            .handle_agent_message_chunk(&json!({
                "messageId": "history-msg-1",
                "text": "stale replay chunk",
            }))
            .await;

        assert!(drain_events(&mut event_rx).is_empty());
        let state = inner.state.lock().await;
        assert!(state.active_message_id.is_none());
        drop(state);
        inner.shutdown().await;
    }

    #[tokio::test]
    async fn handle_tool_call_ignores_replayed_tool_call_ids() {
        let (inner, mut event_rx) = test_kiro_inner().await;
        {
            let mut state = inner.state.lock().await;
            state
                .replay_tool_call_ids
                .insert("tooluse_delayed_replay".to_string());
            state
                .replay_message_ids
                .insert("history-msg-tool".to_string());
        }

        inner
            .handle_notification(
                "session/notification",
                &json!({
                    "type": "toolCall",
                    "messageId": "history-msg-tool",
                    "toolCallId": "tooluse_delayed_replay",
                    "kind": "execute",
                    "title": "Running: sleep 30 && echo done",
                    "rawInput": {
                        "__tool_use_purpose": "Run sleep 30 then print done",
                        "command": "sleep 30 && echo done",
                        "working_dir": "/Users/mike/Tyde"
                    }
                }),
            )
            .await;

        assert!(drain_events(&mut event_rx).is_empty());
        let state = inner.state.lock().await;
        assert!(state.active_tool_contexts.is_empty());
        drop(state);
        inner.shutdown().await;
    }
}
