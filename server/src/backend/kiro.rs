use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use tokio::sync::{Mutex, mpsc, oneshot};

use crate::acp::{
    AcpBridge, AcpInbound, AcpSpawnSpec, acp_mcp_servers_json, extract_message_id,
    extract_text_from_update, extract_tool_call_id, map_plan_status, normalize_update_type,
    parse_tool_call_completion, parse_tool_call_request,
};
use crate::backend::{SessionCommand, StartupMcpServer, render_combined_spawn_instructions};
use crate::process_env;
use crate::subprocess::ImageAttachment;

const KIRO_AGENT_NAME: &str = "kiro";
const KIRO_ADMIN_SESSION_SUBDIR: &str = ".tyde/kiro-admin";
const KIRO_EPHEMERAL_SESSION_SUBDIR: &str = ".tyde/kiro-ephemeral";

struct KiroSpawnMode<'a> {
    ephemeral: bool,
    admin_session: bool,
    initial_model: Option<&'a str>,
    ssh_host: Option<String>,
    startup_mcp_servers: &'a [StartupMcpServer],
    steering_content: Option<&'a str>,
    program_override: Option<String>,
}

#[derive(Clone)]
pub struct KiroCommandHandle {
    inner: Arc<KiroInner>,
}

impl KiroCommandHandle {
    pub async fn execute(&self, command: SessionCommand) -> Result<(), String> {
        self.inner.execute(command).await
    }
}

pub struct KiroSession {
    inner: Arc<KiroInner>,
}

impl KiroSession {
    pub async fn spawn(
        workspace_roots: &[String],
        initial_model: Option<&str>,
        ssh_host: Option<String>,
        startup_mcp_servers: &[StartupMcpServer],
        steering_content: Option<&str>,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Value>), String> {
        Self::spawn_with_mode(
            workspace_roots,
            KiroSpawnMode {
                ephemeral: false,
                admin_session: false,
                initial_model,
                ssh_host,
                startup_mcp_servers,
                steering_content,
                program_override: None,
            },
        )
        .await
    }

    pub async fn spawn_ephemeral(
        workspace_roots: &[String],
        initial_model: Option<&str>,
        ssh_host: Option<String>,
        startup_mcp_servers: &[StartupMcpServer],
        steering_content: Option<&str>,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Value>), String> {
        Self::spawn_with_mode(
            workspace_roots,
            KiroSpawnMode {
                ephemeral: true,
                admin_session: false,
                initial_model,
                ssh_host,
                startup_mcp_servers,
                steering_content,
                program_override: None,
            },
        )
        .await
    }

    pub async fn spawn_admin(
        workspace_roots: &[String],
        initial_model: Option<&str>,
        ssh_host: Option<String>,
        startup_mcp_servers: &[StartupMcpServer],
        steering_content: Option<&str>,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Value>), String> {
        Self::spawn_admin_with_program_override(
            workspace_roots,
            initial_model,
            ssh_host,
            startup_mcp_servers,
            steering_content,
            None,
        )
        .await
    }

    pub async fn spawn_admin_with_program_override(
        workspace_roots: &[String],
        initial_model: Option<&str>,
        ssh_host: Option<String>,
        startup_mcp_servers: &[StartupMcpServer],
        steering_content: Option<&str>,
        program_override: Option<String>,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Value>), String> {
        Self::spawn_with_mode(
            workspace_roots,
            KiroSpawnMode {
                ephemeral: true,
                admin_session: true,
                initial_model,
                ssh_host,
                startup_mcp_servers,
                steering_content,
                program_override,
            },
        )
        .await
    }

    async fn spawn_with_mode(
        workspace_roots: &[String],
        mode: KiroSpawnMode<'_>,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Value>), String> {
        let roots = resolve_kiro_session_roots(
            workspace_roots,
            mode.ssh_host.as_deref(),
            mode.admin_session,
            mode.ephemeral,
        )
        .await?;
        let acp_args: Vec<&str> = vec!["acp"];

        let mut spawn_spec = AcpSpawnSpec::new("Kiro ACP", "kiro-cli-chat", &acp_args)
            .with_local_cwd(roots.session_cwd.clone());
        spawn_spec.local_program = mode
            .program_override
            .unwrap_or_else(resolve_kiro_chat_binary);
        if let Some(model) = mode
            .initial_model
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            spawn_spec.local_args.push("--model".to_string());
            spawn_spec.local_args.push(model.to_string());
            spawn_spec.remote_args.push("--model".to_string());
            spawn_spec.remote_args.push(model.to_string());
        }
        if mode.ssh_host.is_some() {
            spawn_spec = spawn_spec.with_remote_cwd(roots.session_cwd.clone());
        }

        let (bridge, inbound_rx) = AcpBridge::spawn(spawn_spec, mode.ssh_host.as_deref()).await?;

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
                "mcpServers": acp_mcp_servers_json(mode.startup_mcp_servers)
            });
            if let Some(content) = mode.steering_content
                && !content.trim().is_empty()
            {
                session_params["systemPrompt"] = Value::String(content.to_string());
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
            ssh_host: mode.ssh_host,
            state: Mutex::new(KiroState {
                session_id,
                workspace_root: roots.scope_root,
                admin_session: mode.admin_session,
                steering_content: mode.steering_content.map(|s| s.to_string()),
                startup_mcp_servers: mode.startup_mcp_servers.to_vec(),
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
                replay_assistant_message_id: None,
                replay_assistant_text: String::new(),
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
            if let Err(e) = inner.event_tx.send(json!({
                "kind": "SessionStarted",
                "data": { "session_id": &state.session_id }
            })) {
                tracing::error!("Failed to emit SessionStarted: {e}");
            }
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
    known_models: Vec<Value>,
    active_message_id: Option<String>,
    active_stream_text: String,
    active_stream_tool_calls: Vec<Value>,
    active_tool_contexts: HashMap<String, KiroToolContext>,
    tool_call_aliases: HashMap<String, String>,
    cancelled: bool,
    replaying_history: bool,
    replay_assistant_message_id: Option<String>,
    replay_assistant_text: String,
}

#[derive(Clone)]
struct PendingToolCompletion {
    tool_name: String,
    tool_result: Value,
    success: bool,
    error: Option<String>,
}

#[derive(Clone)]
struct KiroToolContext {
    tool_name: String,
    tool_type: Value,
    request_emitted: bool,
    pending_completion: Option<PendingToolCompletion>,
}

struct KiroInner {
    bridge: AcpBridge,
    event_tx: mpsc::UnboundedSender<Value>,
    state: Mutex<KiroState>,
    shutting_down: AtomicBool,
    ssh_host: Option<String>,
}

impl KiroInner {
    async fn execute(&self, command: SessionCommand) -> Result<(), String> {
        match command {
            SessionCommand::SendMessage { message, images } => {
                self.emit_user_message_added(&message, images.as_deref());
                self.emit_event(json!({ "kind": "TypingStatusChanged", "data": true }));

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
                        // rejection of a cancelled request, swallow it — the cancel
                        // handler already emitted OperationCancelled + TypingStatusChanged.
                        let mut state = self.state.lock().await;
                        if state.cancelled {
                            state.cancelled = false;
                            return Ok(());
                        }
                        drop(state);
                        self.emit_event(json!({ "kind": "TypingStatusChanged", "data": false }));
                        return Err(err);
                    }
                };

                self.bridge.sync_inbound().await?;

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

                if stop_reason == "cancelled" {
                    self.force_finalize_active_stream_if_any(Some(response.clone()), true)
                        .await;
                    self.emit_event(json!({
                        "kind": "OperationCancelled",
                        "data": { "message": "Operation cancelled" }
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
                    self.force_finalize_active_stream_if_any(Some(response.clone()), true)
                        .await;
                    self.emit_event(json!({ "kind": "Error", "data": message }));
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
                self.force_finalize_active_stream_if_any(None, true).await;
                self.emit_event(json!({
                    "kind": "OperationCancelled",
                    "data": { "message": "Operation cancelled" }
                }));
                Ok(())
            }
            SessionCommand::GetSettings => {
                let state = self.state.lock().await;
                self.emit_event(json!({
                    "kind": "Settings",
                    "data": {
                        "model": state.model,
                        "mode": state.mode,
                    }
                }));
                Ok(())
            }
            SessionCommand::ListSessions => self.list_sessions().await,
            SessionCommand::ResumeSession { session_id } => self.resume_session(session_id).await,
            SessionCommand::DeleteSession { session_id } => self.delete_session(session_id).await,
            SessionCommand::ListProfiles => {
                self.emit_event(json!({
                    "kind": "ProfilesList",
                    "data": { "profiles": [] }
                }));
                Ok(())
            }
            SessionCommand::SwitchProfile { profile_name: _ } => Ok(()),
            SessionCommand::GetModuleSchemas => {
                self.emit_event(json!({
                    "kind": "ModuleSchemas",
                    "data": { "schemas": [] }
                }));
                Ok(())
            }
            SessionCommand::ListModels => {
                let models = self.state.lock().await.known_models.clone();
                self.emit_event(json!({
                    "kind": "ModelsList",
                    "data": { "models": models }
                }));
                Ok(())
            }
            SessionCommand::UpdateSettings {
                settings,
                persist: _,
            } => {
                if let Some(obj) = settings.as_object() {
                    if let Some(model_value) = obj.get("model") {
                        let next_model = normalize_optional_string(model_value);
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

                    if let Some(mode_value) = obj.get("mode").or_else(|| obj.get("modeId")) {
                        let next_mode = normalize_optional_string(mode_value);
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
                }

                let state = self.state.lock().await;
                self.emit_event(json!({
                    "kind": "Settings",
                    "data": {
                        "model": state.model,
                        "mode": state.mode,
                    }
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

        let raw_sessions = match &self.ssh_host {
            Some(host) => load_remote_kiro_sessions(host).await?,
            None => load_local_kiro_sessions().await?,
        };

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

            sessions.push(json!({
                "id": session_id,
                "session_id": session_id,
                "title": title,
                "created_at": last_modified,
                "last_modified": last_modified,
                "last_message_preview": "",
                "workspace_root": cwd,
                "message_count": Value::Null,
                "backend_kind": "kiro",
            }));
        }

        sessions.sort_by(|a, b| {
            let a_ts = a.get("last_modified").and_then(Value::as_u64).unwrap_or(0);
            let b_ts = b.get("last_modified").and_then(Value::as_u64).unwrap_or(0);
            b_ts.cmp(&a_ts)
        });

        self.emit_event(json!({
            "kind": "SessionsList",
            "data": { "sessions": sessions }
        }));
        Ok(())
    }

    async fn delete_session(&self, session_id: String) -> Result<(), String> {
        let normalized = normalize_optional_string(&Value::String(session_id))
            .ok_or("Invalid session id".to_string())?;

        match &self.ssh_host {
            Some(host) => delete_remote_kiro_session(host, &normalized).await,
            None => delete_local_kiro_session(&normalized).await,
        }
    }

    async fn resume_session(&self, session_id: String) -> Result<(), String> {
        let (cwd, startup_mcp_servers) = {
            let mut state = self.state.lock().await;
            state.replaying_history = true;
            state.replay_assistant_message_id = None;
            state.replay_assistant_text.clear();
            (
                state.workspace_root.clone(),
                state.startup_mcp_servers.clone(),
            )
        };

        self.clear_active_stream().await;
        self.emit_event(json!({ "kind": "ConversationCleared" }));
        self.emit_event(json!({ "kind": "TypingStatusChanged", "data": false }));

        // kiro-cli-chat doesn't check PID liveness when reading .lock files,
        // so stale locks from dead processes block session/load. Remove the
        // lock file before attempting to load.
        let _ = match &self.ssh_host {
            Some(host) => clear_remote_kiro_session_lock(host, &session_id).await,
            None => clear_local_kiro_session_lock(&session_id).await,
        };

        let response = match self
            .bridge
            .request(
                "session/load",
                json!({
                    "sessionId": session_id,
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
                state.replay_assistant_message_id = None;
                state.replay_assistant_text.clear();
                self.emit_event(json!({ "kind": "TypingStatusChanged", "data": false }));
                return Err(err);
            }
        };

        {
            let mut state = self.state.lock().await;
            state.session_id = session_id;
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
            state.replaying_history = false;

            // Emit SessionStarted so forward_events sets backend_session_id on resume
            if let Err(e) = self.event_tx.send(json!({
                "kind": "SessionStarted",
                "data": { "session_id": &state.session_id }
            })) {
                tracing::error!("Failed to emit SessionStarted on resume: {e}");
            }
        }

        self.flush_replay_assistant_message().await;
        self.emit_event(json!({ "kind": "TypingStatusChanged", "data": false }));
        Ok(())
    }

    async fn shutdown(&self) {
        self.shutting_down.store(true, Ordering::Release);

        // The SSH ControlMaster keeps the TCP connection alive after the
        // local slave is killed, so the remote kiro-cli-chat never gets
        // EOF and stays running. Kill the remote process explicitly
        // using the PID from its session lock file.
        if let Some(host) = &self.ssh_host {
            let session_id = self.state.lock().await.session_id.clone();
            let cmd = format!(
                "PID=$(grep -oE '[0-9]+' ~/.kiro/sessions/cli/{0}.lock 2>/dev/null | head -1); \
                 [ -n \"$PID\" ] && kill \"$PID\" 2>/dev/null; true",
                crate::remote::shell_quote_arg(&session_id)
            );
            let _ = crate::remote::run_ssh_raw(host, &cmd).await;
        }

        self.bridge.shutdown().await;
    }

    async fn handle_inbound(&self, inbound: AcpInbound) {
        match inbound {
            AcpInbound::Stderr(line) => {
                self.emit_event(json!({ "kind": "SubprocessStderr", "data": line }));
            }
            AcpInbound::Closed { exit_code } => {
                let code = if self.shutting_down.load(Ordering::Acquire) {
                    Some(0)
                } else {
                    exit_code
                };
                self.emit_event(json!({
                    "kind": "SubprocessExit",
                    "data": { "exit_code": code }
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
                        self.emit_event(json!({
                            "kind": "SubprocessStderr",
                            "data": format!("Failed to handle server request '{method}': {err}"),
                        }));
                        let _ = self.bridge.respond_error(id, -32_000, &err).await;
                    }
                }
            }
            AcpInbound::Barrier { ack } => {
                let _ = ack.send(());
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
            "agent_message_chunk" => {
                self.handle_agent_message_chunk(update).await;
            }
            "user_message_chunk" => {
                self.handle_user_message_chunk(update).await;
            }
            "agent_thought_chunk" => {
                self.handle_reasoning_chunk(update).await;
            }
            "tool_call" => {
                self.handle_tool_call(update).await;
            }
            "tool_call_update" => {
                self.handle_tool_call_update(update).await;
            }
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
        let replaying = self.state.lock().await.replaying_history;
        if !replaying {
            return;
        }

        let text = extract_text_from_update(params);
        if text.trim().is_empty() {
            return;
        }

        self.flush_replay_assistant_message().await;
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

    async fn handle_reasoning_chunk(&self, params: &Value) {
        let delta = extract_text_from_update(params);
        if delta.trim().is_empty() {
            return;
        }

        let message_id = {
            let mut state = self.state.lock().await;
            if state.replaying_history {
                return;
            }
            if let Some(id) = extract_kiro_message_id(params)
                && state.active_message_id.is_none()
            {
                state.active_message_id = Some(id.clone());
                state.active_stream_text.clear();
                state.active_stream_tool_calls.clear();
                let model = state.model.clone().unwrap_or_else(|| "kiro".to_string());
                self.emit_event(json!({ "kind": "TypingStatusChanged", "data": true }));
                self.emit_event(json!({
                    "kind": "StreamStart",
                    "data": {
                        "message_id": id,
                        "agent": KIRO_AGENT_NAME,
                        "model": model
                    }
                }));
            }
            state
                .active_message_id
                .clone()
                .unwrap_or_else(|| format!("kiro-msg-{}", unix_now_ms()))
        };

        self.emit_event(json!({
            "kind": "StreamReasoningDelta",
            "data": {
                "message_id": message_id,
                "text": delta
            }
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

        let replay_flush = {
            let mut state = self.state.lock().await;
            if state.replaying_history {
                let replay_message_id = chunk_message_id.clone().unwrap_or_else(|| {
                    state
                        .replay_assistant_message_id
                        .clone()
                        .unwrap_or_else(|| format!("history-{}", unix_now_ms()))
                });

                if let Some(active_id) = state.replay_assistant_message_id.clone() {
                    if active_id != replay_message_id {
                        let flushed =
                            Some((active_id, std::mem::take(&mut state.replay_assistant_text)));
                        state.replay_assistant_message_id = Some(replay_message_id.clone());
                        state.replay_assistant_text.push_str(&delta);
                        Some(flushed)
                    } else {
                        state.replay_assistant_text.push_str(&delta);
                        Some(None)
                    }
                } else {
                    state.replay_assistant_message_id = Some(replay_message_id.clone());
                    state.replay_assistant_text.push_str(&delta);
                    Some(None)
                }
            } else {
                None
            }
        };

        if let Some(flushed) = replay_flush {
            if let Some(replay) = flushed {
                self.emit_replay_message(Some(replay)).await;
            }
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
            let mut previous_stream: Option<(String, String, Vec<Value>)> = None;

            if let Some(next_id) = chunk_message_id.clone()
                && let Some(active_id) = state.active_message_id.clone()
                && active_id != next_id
            {
                let previous_text = std::mem::take(&mut state.active_stream_text);
                let previous_tool_calls = std::mem::take(&mut state.active_stream_tool_calls);
                if has_renderable_stream_text(&previous_text) || !previous_tool_calls.is_empty() {
                    previous_stream = Some((active_id, previous_text, previous_tool_calls));
                }
                state.active_message_id = None;
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
            self.emit_stream_end(
                prev_message_id,
                prev_text,
                None,
                prev_tool_calls,
                false,
                false,
            )
            .await;
        }

        if let Some(start_message_id) = started_message_id {
            self.emit_event(json!({ "kind": "TypingStatusChanged", "data": true }));
            self.emit_event(json!({
                "kind": "StreamStart",
                "data": {
                    "message_id": start_message_id,
                    "agent": KIRO_AGENT_NAME,
                    "model": model,
                }
            }));
        }

        self.emit_event(json!({
            "kind": "StreamDelta",
            "data": {
                "message_id": stream_message_id,
                "text": delta,
            }
        }));
    }

    async fn handle_tool_call(&self, params: &Value) {
        let Some(request) = parse_tool_call_request(params) else {
            self.emit_event(json!({
                "kind": "SubprocessStderr",
                "data": format!("Ignoring ACP tool_call without toolCallId: {params}"),
            }));
            return;
        };
        let raw_tool_call_id = normalize_tool_call_id_fragment(&request.tool_call_id);

        let incoming_message_id = extract_kiro_message_id(params);
        let workspace_root = self.state.lock().await.workspace_root.clone();

        let mut start_event: Option<(String, String)> = None;
        let mut previous_stream: Option<(String, String, Vec<Value>)> = None;
        let mut should_finalize_current = false;
        let mut refresh_tool_request: Option<(String, String, Value)> = None;
        {
            let mut state = self.state.lock().await;

            if let Some(next_id) = incoming_message_id.clone()
                && let Some(active_id) = state.active_message_id.clone()
                && active_id != next_id
            {
                let previous_text = std::mem::take(&mut state.active_stream_text);
                let previous_tool_calls = std::mem::take(&mut state.active_stream_tool_calls);
                if has_renderable_stream_text(&previous_text) || !previous_tool_calls.is_empty() {
                    previous_stream = Some((active_id, previous_text, previous_tool_calls));
                }
                state.active_message_id = None;
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

                let tool_call_entry = json!({
                    "id": canonical_id.clone(),
                    "name": request.tool_name.clone(),
                    "arguments": request.args.clone(),
                });
                let already_present = state.active_stream_tool_calls.iter().any(|call| {
                    call.get("id").and_then(Value::as_str) == Some(canonical_id.as_str())
                });
                if !already_present {
                    state.active_stream_tool_calls.push(tool_call_entry);
                }
                should_finalize_current = true;
            }
        };

        if let Some((prev_message_id, prev_text, prev_tool_calls)) = previous_stream {
            self.emit_stream_end(
                prev_message_id,
                prev_text,
                None,
                prev_tool_calls,
                false,
                false,
            )
            .await;
        }

        if let Some((message_id, model)) = start_event {
            self.emit_event(json!({ "kind": "TypingStatusChanged", "data": true }));
            self.emit_event(json!({
                "kind": "StreamStart",
                "data": {
                    "message_id": message_id,
                    "agent": KIRO_AGENT_NAME,
                    "model": model,
                }
            }));
        }

        if should_finalize_current {
            self.finalize_active_stream_if_any(None, false).await;
        }

        if let Some((tool_call_id, tool_name, tool_type)) = refresh_tool_request {
            self.emit_event(json!({
                "kind": "ToolRequest",
                "data": {
                    "tool_call_id": tool_call_id,
                    "tool_name": tool_name,
                    "tool_type": tool_type,
                }
            }));
        }
    }

    async fn handle_tool_call_update(&self, params: &Value) {
        let raw_tool_call_id =
            extract_kiro_tool_call_id(params).map(|raw| normalize_tool_call_id_fragment(&raw));
        let message_id = extract_kiro_message_id(params);

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
                let kind = context
                    .tool_type
                    .get("kind")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if kind != "ModifyFile" {
                    None
                } else {
                    let file_path = context
                        .tool_type
                        .get("file_path")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let before = context
                        .tool_type
                        .get("before")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let after = context
                        .tool_type
                        .get("after")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    if file_path.is_empty() || !has_visible_text(before) || has_visible_text(after)
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

        let mut emit_completion_now: Option<(String, String, Value, bool, Option<String>)> = None;
        let mut refresh_tool_request: Option<(String, String, Value)> = None;
        {
            let mut state = self.state.lock().await;
            if let Some(context) = state.active_tool_contexts.get_mut(&completion.tool_call_id) {
                if let Some(after_contents) = backfilled_after_contents.clone() {
                    let current_after = context
                        .tool_type
                        .get("after")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    if current_after != after_contents {
                        if let Some(obj) = context.tool_type.as_object_mut() {
                            obj.insert("after".to_string(), Value::String(after_contents));
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
            self.emit_event(json!({
                "kind": "ToolRequest",
                "data": {
                    "tool_call_id": tool_call_id,
                    "tool_name": tool_name,
                    "tool_type": tool_type,
                }
            }));
        }

        if let Some((tool_call_id, tool_name, tool_result, success, error)) = emit_completion_now {
            self.emit_event(json!({
                "kind": "ToolExecutionCompleted",
                "data": {
                    "tool_call_id": tool_call_id,
                    "tool_name": tool_name,
                    "tool_result": tool_result,
                    "success": success,
                    "error": error,
                }
            }));
        }
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

                json!({
                    "id": index as u64 + 1,
                    "description": description,
                    "status": status,
                })
            })
            .collect::<Vec<_>>();

        self.emit_event(json!({
            "kind": "TaskUpdate",
            "data": {
                "title": title,
                "tasks": tasks,
            }
        }));
    }

    async fn handle_error_notification(&self, params: &Value) {
        let message = params
            .get("message")
            .or_else(|| params.get("error").and_then(|v| v.get("message")))
            .and_then(Value::as_str)
            .unwrap_or("Kiro error")
            .to_string();

        self.clear_active_stream().await;
        self.emit_event(json!({ "kind": "TypingStatusChanged", "data": false }));
        self.emit_event(json!({ "kind": "Error", "data": message }));
    }

    async fn emit_replay_message(&self, replay: Option<(String, String)>) {
        let Some((_, text)) = replay else {
            return;
        };
        let text = text.trim().to_string();
        if text.is_empty() {
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

        self.emit_event(json!({
            "kind": "MessageAdded",
            "data": {
                "timestamp": unix_now_ms(),
                "sender": { "Assistant": { "agent": KIRO_AGENT_NAME } },
                "content": text,
                "tool_calls": [],
                "model_info": { "model": model },
                "images": [],
            }
        }));
    }

    async fn flush_replay_assistant_message(&self) {
        let replay = {
            let mut state = self.state.lock().await;
            state
                .replay_assistant_message_id
                .take()
                .map(|message_id| (message_id, std::mem::take(&mut state.replay_assistant_text)))
        };
        self.emit_replay_message(replay).await;
    }

    async fn finalize_active_stream_if_any(&self, usage: Option<Value>, end_typing: bool) {
        self.finalize_active_stream_if_any_with_mode(usage, false, end_typing)
            .await;
    }

    async fn force_finalize_active_stream_if_any(&self, usage: Option<Value>, end_typing: bool) {
        self.finalize_active_stream_if_any_with_mode(usage, true, end_typing)
            .await;
    }

    async fn finalize_active_stream_if_any_with_mode(
        &self,
        usage: Option<Value>,
        force_emit: bool,
        end_typing: bool,
    ) {
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
            self.emit_stream_end(message_id, text, usage, tool_calls, force_emit, end_typing)
                .await;
        } else if end_typing {
            self.emit_event(json!({ "kind": "TypingStatusChanged", "data": false }));
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
        tool_calls: Vec<Value>,
        force_emit: bool,
        end_typing: bool,
    ) {
        let cleaned_text = strip_ansi_and_controls(&text);
        if !force_emit && !has_renderable_stream_text(&cleaned_text) && tool_calls.is_empty() {
            if end_typing {
                self.emit_event(json!({ "kind": "TypingStatusChanged", "data": false }));
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
            .unwrap_or(Value::Null);
        let tool_calls_for_events = tool_calls.clone();

        self.emit_event(json!({
            "kind": "StreamEnd",
            "data": {
                "message": {
                    "timestamp": unix_now_ms(),
                    "sender": { "Assistant": { "agent": KIRO_AGENT_NAME } },
                    "content": cleaned_text,
                    "tool_calls": tool_calls,
                    "model_info": { "model": model },
                    "token_usage": normalized_usage,
                    "context_breakdown": context_breakdown,
                    "images": [],
                }
            }
        }));
        self.flush_tool_events_after_stream_end(&tool_calls_for_events)
            .await;
        if end_typing {
            self.emit_event(json!({ "kind": "TypingStatusChanged", "data": false }));
        }
    }

    async fn flush_tool_events_after_stream_end(&self, tool_calls: &[Value]) {
        let mut completions_to_emit: Vec<(String, String, Value, bool, Option<String>)> =
            Vec::new();
        let mut requests_to_emit: Vec<(String, String, Value)> = Vec::new();

        {
            let mut state = self.state.lock().await;
            for tool_call in tool_calls {
                let Some(tool_call_id) = tool_call
                    .get("id")
                    .and_then(Value::as_str)
                    .map(|value| value.to_string())
                else {
                    continue;
                };

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
            self.emit_event(json!({
                "kind": "ToolRequest",
                "data": {
                    "tool_call_id": tool_call_id,
                    "tool_name": tool_name,
                    "tool_type": tool_type,
                }
            }));
        }

        for (tool_call_id, tool_name, tool_result, success, error) in completions_to_emit {
            self.emit_event(json!({
                "kind": "ToolExecutionCompleted",
                "data": {
                    "tool_call_id": tool_call_id,
                    "tool_name": tool_name,
                    "tool_result": tool_result,
                    "success": success,
                    "error": error,
                }
            }));
        }
    }

    fn emit_user_message_added(&self, content: &str, images: Option<&[ImageAttachment]>) {
        let image_payload = images
            .unwrap_or(&[])
            .iter()
            .map(|image| {
                json!({
                    "media_type": image.media_type,
                    "data": image.data,
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
                "images": image_payload,
            }
        }));
    }

    fn emit_event(&self, event: Value) {
        if let Err(err) = self.event_tx.send(event) {
            tracing::trace!("event send failed: {err}");
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
    ssh_host: Option<&str>,
    admin_session: bool,
    ephemeral: bool,
) -> Result<KiroSessionRoots, String> {
    if let Some(host) = ssh_host {
        let parsed = crate::remote::parse_remote_workspace_roots(workspace_roots)?
            .ok_or("Expected remote workspace roots for SSH session")?;
        let scope_root = parsed
            .1
            .into_iter()
            .next()
            .ok_or("No remote workspace root found")?;
        let session_cwd = if admin_session {
            join_posix_path(&scope_root, KIRO_ADMIN_SESSION_SUBDIR)
        } else if ephemeral {
            join_posix_path(&scope_root, KIRO_EPHEMERAL_SESSION_SUBDIR)
        } else {
            scope_root.clone()
        };
        if admin_session || ephemeral {
            ensure_remote_directory(host, &session_cwd).await?;
        }
        return Ok(KiroSessionRoots {
            session_cwd,
            scope_root,
        });
    }

    let scope_root = pick_workspace_root(workspace_roots)?;
    let session_cwd = if admin_session {
        let dir = PathBuf::from(&scope_root).join(".tyde").join("kiro-admin");
        tokio::fs::create_dir_all(&dir).await.map_err(|err| {
            format!(
                "Failed to create Kiro admin directory '{}': {err}",
                dir.display()
            )
        })?;
        dir.to_string_lossy().to_string()
    } else if ephemeral {
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

async fn ensure_remote_directory(host: &str, dir: &str) -> Result<(), String> {
    let command = format!("mkdir -p {}", crate::remote::shell_quote_arg(dir));
    let output = crate::remote::run_ssh_raw(host, &command).await?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let detail = if stderr.is_empty() {
        format!("exit status {}", output.status)
    } else {
        stderr
    };
    Err(format!(
        "Failed to create remote Kiro admin directory '{dir}' on '{host}': {detail}"
    ))
}

fn join_posix_path(base: &str, suffix: &str) -> String {
    let base = base.trim_end_matches('/');
    let suffix = suffix.trim_start_matches('/');
    if base.is_empty() {
        format!("/{}", suffix)
    } else {
        format!("{base}/{suffix}")
    }
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
async fn map_tool_request_type(params: &Value, args: &Value, workspace_root: &str) -> Value {
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
            json!({
                "kind": "RunCommand",
                "command": command,
                "working_directory": working_directory,
            })
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
                && let Ok(contents) = tokio::fs::read_to_string(&resolved_file_path).await
            {
                before = contents;
            }

            json!({
                "kind": "ModifyFile",
                "file_path": file_path,
                "before": before,
                "after": after,
            })
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
            json!({
                "kind": "ReadFiles",
                "file_paths": file_paths,
            })
        }
        _ => json!({
            "kind": "Other",
            "args": args,
        }),
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
) -> Value {
    if !completion.success {
        let short_message = completion
            .error
            .clone()
            .unwrap_or_else(|| format!("{} failed", completion.tool_name));
        let detailed_message = serde_json::to_string_pretty(&completion.tool_result)
            .unwrap_or_else(|_| completion.tool_result.to_string());
        return json!({
            "kind": "Error",
            "short_message": short_message,
            "detailed_message": detailed_message,
        });
    }

    match completion.kind.as_str() {
        "execute" => {
            let json_obj = extract_first_item_json(&completion.tool_result);
            let exit_code = json_obj
                .and_then(|obj| obj.get("exit_status").and_then(Value::as_str))
                .and_then(|s| s.rsplit(':').next())
                .and_then(|n| n.trim().parse::<i64>().ok())
                .unwrap_or(0);
            let stdout = json_obj
                .and_then(|obj| obj.get("stdout").and_then(Value::as_str))
                .unwrap_or("")
                .to_string();
            let stderr = json_obj
                .and_then(|obj| obj.get("stderr").and_then(Value::as_str))
                .unwrap_or("")
                .to_string();
            json!({
                "kind": "RunCommand",
                "exit_code": exit_code,
                "stdout": stdout,
                "stderr": stderr,
            })
        }
        "edit" => {
            let before = context
                .and_then(|ctx| ctx.tool_type.get("before"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            let after = context
                .and_then(|ctx| ctx.tool_type.get("after"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            let (lines_added, lines_removed) = estimate_line_diff_counts(before, after);
            json!({
                "kind": "ModifyFile",
                "lines_added": lines_added,
                "lines_removed": lines_removed,
            })
        }
        "read" => {
            let file_paths = context
                .and_then(|ctx| ctx.tool_type.get("file_paths"))
                .and_then(Value::as_array);
            let text_len = extract_first_item_text(&completion.tool_result)
                .map(|t| t.len() as u64)
                .unwrap_or(0);
            let files: Vec<Value> = file_paths
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(|path| json!({ "path": path, "bytes": text_len }))
                .collect();
            json!({
                "kind": "ReadFiles",
                "files": files,
            })
        }
        _ => json!({
            "kind": "Other",
            "result": completion.tool_result,
        }),
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
                if let Some(parsed) = parse_json_value_from_string(child)
                    && let Some(found) =
                        extract_first_string_recursive(&parsed, keys, depth + 1, max_depth)
                {
                    return Some(found);
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
                if let Some(parsed) = parse_json_value_from_string(child)
                    && let Some(found) =
                        extract_first_string_recursive(&parsed, keys, depth + 1, max_depth)
                {
                    return Some(found);
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

fn usage_u64(value: &Value, keys: &[&str]) -> Option<u64> {
    for key in keys {
        let Some(raw) = value.get(*key) else {
            continue;
        };
        if let Some(number) = raw.as_u64() {
            return Some(number);
        }
        if let Some(number) = raw.as_i64()
            && number >= 0
        {
            return Some(number as u64);
        }
        if let Some(text) = raw.as_str()
            && let Ok(parsed) = text.trim().parse::<u64>()
        {
            return Some(parsed);
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
    let conversation_history_bytes = remaining;

    json!({
        "system_prompt_bytes": system_prompt_bytes,
        "tool_io_bytes": tool_io_bytes,
        "conversation_history_bytes": conversation_history_bytes,
        "reasoning_bytes": reasoning_bytes,
        "context_injection_bytes": 0,
        "input_tokens": input_tokens,
        "context_window": context_window,
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

fn extract_known_models(value: &Value) -> Vec<Value> {
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

    let mut deduped: Vec<Value> = Vec::new();
    let mut indexes = HashMap::new();

    for model in &raw_models {
        let Some(id) = model
            .get("id")
            .or_else(|| model.get("modelId"))
            .or_else(|| model.get("name"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|raw| !raw.is_empty())
        else {
            continue;
        };
        let display_name = model
            .get("name")
            .or_else(|| model.get("displayName"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|raw| !raw.is_empty())
            .unwrap_or(id);
        let is_default = model
            .get("isDefault")
            .or_else(|| model.get("default"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let normalized_id = id.to_ascii_lowercase();
        let preferred_id = id.to_string();

        match indexes.get(&normalized_id).copied() {
            Some(index) => {
                let existing = deduped
                    .get_mut(index)
                    .and_then(Value::as_object_mut)
                    .expect("deduped Kiro model entry must be an object");
                if preferred_id == normalized_id {
                    existing.insert("id".to_string(), Value::String(normalized_id.clone()));
                }
                if is_default {
                    existing.insert("isDefault".to_string(), Value::Bool(true));
                }
            }
            None => {
                let id_value = if id == normalized_id {
                    normalized_id.clone()
                } else {
                    preferred_id
                };
                indexes.insert(normalized_id, deduped.len());
                deduped.push(json!({
                    "id": id_value,
                    "displayName": display_name,
                    "isDefault": is_default,
                }));
            }
        }
    }

    deduped
}

fn session_settings_schema_from_known_models(
    known_models: &[Value],
) -> Result<protocol::SessionSettingsSchema, String> {
    let mut options = Vec::new();
    let mut default = None;

    for model in known_models {
        let id = model
            .get("id")
            .or_else(|| model.get("modelId"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| format!("Kiro model entry missing id: {model}"))?;
        let label = model
            .get("displayName")
            .or_else(|| model.get("name"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(id);
        if model
            .get("isDefault")
            .or_else(|| model.get("default"))
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            default = Some(id.to_string());
        }
        options.push(protocol::SelectOption {
            value: id.to_string(),
            label: label.to_string(),
        });
    }

    if options.is_empty() {
        return Err("Kiro reported no selectable models".to_string());
    }

    Ok(protocol::SessionSettingsSchema {
        backend_kind: protocol::BackendKind::Kiro,
        fields: vec![protocol::SessionSettingField {
            key: "model".to_string(),
            label: "Model".to_string(),
            description: None,
            use_slider: false,
            field_type: protocol::SessionSettingFieldType::Select {
                options,
                default,
                nullable: true,
            },
        }],
    })
}

pub(crate) async fn probe_session_settings_schema(
    workspace_roots: &[String],
    program_override: Option<String>,
) -> Result<protocol::SessionSettingsSchema, String> {
    let (session, mut raw_events) = KiroSession::spawn_admin_with_program_override(
        workspace_roots,
        None,
        None,
        &[],
        None,
        program_override,
    )
    .await?;
    let handle = session.command_handle();

    let probe_result = async {
        handle.execute(SessionCommand::ListModels).await?;
        loop {
            let raw = tokio::time::timeout(Duration::from_secs(5), raw_events.recv())
                .await
                .map_err(|_| "Timed out waiting for Kiro ModelsList event".to_string())?
                .ok_or_else(|| "Kiro admin probe ended before ModelsList".to_string())?;
            if raw.get("kind").and_then(Value::as_str) != Some("ModelsList") {
                continue;
            }
            let known_models = raw
                .get("data")
                .and_then(|data| data.get("models"))
                .and_then(Value::as_array)
                .ok_or_else(|| format!("Kiro ModelsList missing data.models array: {raw}"))?;
            return session_settings_schema_from_known_models(known_models);
        }
    }
    .await;

    session.shutdown().await;
    probe_result
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
    process_env::find_executable_in_path(binary).map(|path| path.to_string_lossy().to_string())
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
    if let Ok(pid) = content.trim().parse::<u32>()
        && is_pid_alive(pid)
    {
        return Ok(());
    }
    tokio::fs::remove_file(&lock_path)
        .await
        .map_err(|err| format!("Failed to remove stale lock {}: {err}", lock_path.display()))?;
    Ok(())
}

async fn clear_remote_kiro_session_lock(host: &str, session_id: &str) -> Result<(), String> {
    let cmd = format!(
        "LOCKFILE=~/.kiro/sessions/cli/{0}.lock; \
         if [ -f \"$LOCKFILE\" ]; then \
           PID=$(grep -oE '[0-9]+' \"$LOCKFILE\" | head -1); \
           if [ -n \"$PID\" ] && ! kill -0 \"$PID\" 2>/dev/null; then \
             rm -f \"$LOCKFILE\"; \
           fi; \
         fi",
        crate::remote::shell_quote_arg(session_id)
    );
    let output = crate::remote::run_ssh_raw(host, &cmd).await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("Failed to clear remote session lock: {stderr}"));
    }
    Ok(())
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

async fn delete_remote_kiro_session(host: &str, session_id: &str) -> Result<(), String> {
    let cmd = format!(
        "rm -f ~/.kiro/sessions/cli/{0}.json ~/.kiro/sessions/cli/{0}.jsonl ~/.kiro/sessions/cli/{0}.lock",
        crate::remote::shell_quote_arg(session_id)
    );
    let output = crate::remote::run_ssh_raw(host, &cmd).await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("Failed to delete remote kiro session: {stderr}"));
    }
    Ok(())
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

async fn load_remote_kiro_sessions(host: &str) -> Result<Vec<(String, Value)>, String> {
    let cmd = concat!(
        "for f in ~/.kiro/sessions/cli/*.json; do ",
        "[ -f \"$f\" ] && ",
        "printf 'TYDE_SID:%s\n' \"$(basename \"$f\" .json)\" && ",
        "cat \"$f\" && ",
        "printf '\nTYDE_SEND\n'; ",
        "done"
    );
    let output = crate::remote::run_ssh_raw(host, cmd).await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("Failed to list remote kiro sessions: {stderr}"));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_remote_session_dump(&stdout)
}

fn parse_remote_session_dump(dump: &str) -> Result<Vec<(String, Value)>, String> {
    let mut result = Vec::new();
    let mut current_id: Option<String> = None;
    let mut current_content = String::new();

    for line in dump.lines() {
        if let Some(id) = line.strip_prefix("TYDE_SID:") {
            if let Some(prev_id) = current_id.take()
                && let Ok(metadata) = serde_json::from_str::<Value>(&current_content)
            {
                result.push((prev_id, metadata));
            }
            current_id = Some(id.trim().to_string());
            current_content.clear();
        } else if line == "TYDE_SEND" {
            if let Some(id) = current_id.take()
                && let Ok(metadata) = serde_json::from_str::<Value>(&current_content)
            {
                result.push((id, metadata));
            }
            current_content.clear();
        } else if current_id.is_some() {
            if !current_content.is_empty() {
                current_content.push('\n');
            }
            current_content.push_str(line);
        }
    }
    if let Some(id) = current_id
        && let Ok(metadata) = serde_json::from_str::<Value>(&current_content)
    {
        result.push((id, metadata));
    }
    Ok(result)
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
    if let Some(s) = ts_field.and_then(Value::as_str)
        && let Some(ms) = parse_iso8601_to_unix_ms(s)
    {
        return ms;
    }
    ts_field.and_then(Value::as_u64).unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Backend trait implementation
// ---------------------------------------------------------------------------

use protocol::{
    AgentInput, BackendKind, ChatEvent, ChatMessage, MessageSender, ModelInfo, SessionId,
    SessionSettingValue, SpawnCostHint, StreamEndData, StreamStartData, StreamTextDeltaData,
};

use super::{
    Backend, BackendSession, BackendSpawnConfig, EventStream, empty_session_settings_schema,
    protocol_images_to_attachments, resolve_settings as resolve_backend_settings,
    session_settings_to_json,
};

const BACKEND_EVENT_BUFFER: usize = 256;
const BACKEND_INPUT_BUFFER: usize = 64;
const BACKEND_AGENT_NAME: &str = "kiro";

pub struct KiroBackend {
    input_tx: mpsc::Sender<AgentInput>,
    interrupt_tx: mpsc::Sender<()>,
    events_tx: mpsc::Sender<ChatEvent>,
    session_id: Arc<std::sync::Mutex<Option<SessionId>>>,
}

fn kiro_backend_model(cost_hint: Option<SpawnCostHint>) -> Option<&'static str> {
    match cost_hint {
        Some(SpawnCostHint::Low) => Some("claude-haiku-4.5"),
        Some(SpawnCostHint::Medium) => Some("claude-sonnet-4"),
        Some(SpawnCostHint::High) => Some("claude-sonnet-4.5"),
        None => None,
    }
}

fn kiro_cost_hint_defaults(cost_hint: SpawnCostHint) -> protocol::SessionSettingsValues {
    let mut values = protocol::SessionSettingsValues::default();
    if let Some(model) = kiro_backend_model(Some(cost_hint)) {
        values.0.insert(
            "model".to_string(),
            SessionSettingValue::String(model.to_string()),
        );
    }
    values
}

pub(crate) fn resolve_session_settings(
    config: &BackendSpawnConfig,
) -> protocol::SessionSettingsValues {
    resolve_backend_settings(
        config,
        &KiroBackend::session_settings_schema(),
        kiro_cost_hint_defaults,
    )
}

fn backend_user_message(content: String, images: Option<Vec<protocol::ImageData>>) -> ChatEvent {
    ChatEvent::MessageAdded(ChatMessage {
        timestamp: unix_now_ms(),
        sender: MessageSender::User,
        content,
        reasoning: None,
        tool_calls: Vec::new(),
        model_info: None,
        token_usage: None,
        context_breakdown: None,
        images,
    })
}

impl Backend for KiroBackend {
    fn session_settings_schema() -> protocol::SessionSettingsSchema {
        empty_session_settings_schema(BackendKind::Kiro)
    }

    async fn spawn(
        workspace_roots: Vec<String>,
        config: BackendSpawnConfig,
        initial_input: protocol::SendMessagePayload,
    ) -> Result<(Self, EventStream), String> {
        let initial_message = initial_input.message;
        let initial_images = protocol_images_to_attachments(initial_input.images);
        let (input_tx, mut input_rx) = mpsc::channel::<AgentInput>(BACKEND_INPUT_BUFFER);
        let (interrupt_tx, mut interrupt_rx) = mpsc::channel::<()>(BACKEND_INPUT_BUFFER);
        let (events_tx, events_rx) = mpsc::channel::<ChatEvent>(BACKEND_EVENT_BUFFER);
        let events_tx_task = events_tx.clone();
        let session_id = Arc::new(std::sync::Mutex::new(None));
        let session_id_task = Arc::clone(&session_id);
        let (ready_tx, ready_rx) = oneshot::channel::<Result<(), String>>();

        tokio::spawn(async move {
            let mut ready_tx: Option<oneshot::Sender<Result<(), String>>> = Some(ready_tx);
            let roots = if workspace_roots.is_empty() {
                vec!["/tmp".to_string()]
            } else {
                workspace_roots
            };
            let combined_instructions =
                render_combined_spawn_instructions(&config.resolved_spawn_config);
            let (session, mut raw_events) = match KiroSession::spawn(
                &roots,
                None,
                None,
                &config.startup_mcp_servers,
                combined_instructions.as_deref(),
            )
            .await
            {
                Ok(v) => v,
                Err(err) => {
                    tracing::error!("Failed to spawn Kiro session: {err}");
                    if let Some(tx) = ready_tx.take() {
                        let _ = tx.send(Err(format!("Failed to spawn Kiro session: {err}")));
                    }
                    return;
                }
            };
            *session_id_task
                .lock()
                .expect("kiro session_id mutex poisoned") = Some(SessionId(
                session.inner.state.lock().await.session_id.clone(),
            ));

            let handle = session.command_handle();
            let resolved_settings = resolve_session_settings(&config);
            let model_override = match resolved_settings.0.get("model") {
                Some(SessionSettingValue::String(value)) => Some(value.clone()),
                _ => None,
            };
            if model_override.is_some()
                && let Err(err) = handle
                    .execute(SessionCommand::UpdateSettings {
                        settings: session_settings_to_json(&resolved_settings),
                        persist: false,
                    })
                    .await
            {
                tracing::error!("Failed to configure Kiro session: {err}");
                if let Some(tx) = ready_tx.take() {
                    let _ = tx.send(Err(format!("Failed to configure Kiro session: {err}")));
                }
                session.shutdown().await;
                return;
            }
            if let Some(tx) = ready_tx.take() {
                let _ = tx.send(Ok(()));
            }

            let events_tx_forward = events_tx_task.clone();
            let forward_task = tokio::spawn(async move {
                while let Some(raw) = raw_events.recv().await {
                    if let Some(event) = map_kiro_value_to_chat_event(&raw)
                        && events_tx_forward.send(event).await.is_err()
                    {
                        return;
                    }
                }
            });

            let (command_error_tx, mut command_error_rx) = mpsc::unbounded_channel::<String>();
            let initial_handle = handle.clone();
            let initial_command_error_tx = command_error_tx.clone();
            tokio::spawn(async move {
                if let Err(err) = initial_handle
                    .execute(SessionCommand::SendMessage {
                        message: initial_message,
                        images: initial_images,
                    })
                    .await
                {
                    let _ = initial_command_error_tx
                        .send(format!("Failed to send initial Kiro prompt: {err}"));
                }
            });

            loop {
                tokio::select! {
                    maybe_error = command_error_rx.recv() => {
                        let Some(error) = maybe_error else {
                            break;
                        };
                        tracing::error!("{error}");
                        break;
                    }
                    input = input_rx.recv() => {
                        let Some(input) = input else { break };
                        match input {
                            AgentInput::SendMessage(payload) => {
                                let message = payload.message;
                                let images = protocol_images_to_attachments(payload.images);
                                let handle = handle.clone();
                                let command_error_tx = command_error_tx.clone();
                                tokio::spawn(async move {
                                    if let Err(err) = handle
                                        .execute(SessionCommand::SendMessage {
                                            message,
                                            images,
                                        })
                                        .await
                                    {
                                        let _ = command_error_tx.send(format!(
                                            "Failed to send Kiro follow-up prompt: {err}"
                                        ));
                                    }
                                });
                            }
                            AgentInput::UpdateSessionSettings(payload) => {
                                if let Err(err) = handle
                                    .execute(SessionCommand::UpdateSettings {
                                        settings: session_settings_to_json(&payload.values),
                                        persist: false,
                                    })
                                    .await
                                {
                                    tracing::error!("Failed to update Kiro session settings: {err}");
                                    break;
                                }
                            }
                            AgentInput::EditQueuedMessage(_)
                            | AgentInput::CancelQueuedMessage(_)
                            | AgentInput::SendQueuedMessageNow(_) => {
                                panic!(
                                    "queued-message inputs must be handled by the agent actor before reaching the backend"
                                );
                            }
                        }
                    }
                    interrupt = interrupt_rx.recv() => {
                        let Some(()) = interrupt else { break };
                        if let Err(err) = handle.execute(SessionCommand::CancelConversation).await {
                            tracing::error!("Failed to interrupt Kiro turn: {err}");
                            break;
                        }
                    }
                }
            }

            session.shutdown().await;
            let _ = forward_task.await;
        });

        match ready_rx.await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => return Err(err),
            Err(_) => return Err("Kiro spawn initialization task ended early".to_string()),
        }

        Ok((
            Self {
                input_tx,
                interrupt_tx,
                events_tx: events_tx.clone(),
                session_id,
            },
            EventStream::new(events_rx),
        ))
    }

    async fn resume(
        workspace_roots: Vec<String>,
        config: BackendSpawnConfig,
        session_id: SessionId,
    ) -> Result<(Self, EventStream), String> {
        let (input_tx, mut input_rx) = mpsc::channel::<AgentInput>(BACKEND_INPUT_BUFFER);
        let (interrupt_tx, mut interrupt_rx) = mpsc::channel::<()>(BACKEND_INPUT_BUFFER);
        let (events_tx, events_rx) = mpsc::channel::<ChatEvent>(BACKEND_EVENT_BUFFER);
        let events_tx_task = events_tx.clone();
        let known_session_id = Arc::new(std::sync::Mutex::new(Some(session_id.clone())));
        let known_session_id_task = Arc::clone(&known_session_id);

        tokio::spawn(async move {
            let roots = if workspace_roots.is_empty() {
                vec!["/tmp".to_string()]
            } else {
                workspace_roots
            };
            let combined_instructions =
                render_combined_spawn_instructions(&config.resolved_spawn_config);
            let (session, mut raw_events) = match KiroSession::spawn(
                &roots,
                None,
                None,
                &config.startup_mcp_servers,
                combined_instructions.as_deref(),
            )
            .await
            {
                Ok(v) => v,
                Err(err) => {
                    tracing::error!("Failed to spawn Kiro resume session: {err}");
                    return;
                }
            };

            let handle = session.command_handle();
            if let Err(err) = handle
                .execute(SessionCommand::ResumeSession {
                    session_id: session_id.0.clone(),
                })
                .await
            {
                tracing::error!("Failed to resume Kiro session: {err}");
                session.shutdown().await;
                return;
            }
            *known_session_id_task
                .lock()
                .expect("kiro session_id mutex poisoned") = Some(session_id);

            let resolved_settings = resolve_session_settings(&config);
            let model_override = match resolved_settings.0.get("model") {
                Some(SessionSettingValue::String(value)) => Some(value.clone()),
                _ => None,
            };
            if model_override.is_some()
                && let Err(err) = handle
                    .execute(SessionCommand::UpdateSettings {
                        settings: session_settings_to_json(&resolved_settings),
                        persist: false,
                    })
                    .await
            {
                tracing::error!("Failed to configure resumed Kiro session: {err}");
                session.shutdown().await;
                return;
            }

            let events_tx_forward = events_tx_task.clone();
            let forward_task = tokio::spawn(async move {
                while let Some(raw) = raw_events.recv().await {
                    if let Some(event) = map_kiro_value_to_chat_event(&raw)
                        && events_tx_forward.send(event).await.is_err()
                    {
                        return;
                    }
                }
            });

            let (command_error_tx, mut command_error_rx) = mpsc::unbounded_channel::<String>();
            loop {
                tokio::select! {
                    maybe_error = command_error_rx.recv() => {
                        let Some(error) = maybe_error else {
                            break;
                        };
                        tracing::error!("{error}");
                        break;
                    }
                    input = input_rx.recv() => {
                        let Some(input) = input else { break };
                        match input {
                            AgentInput::SendMessage(payload) => {
                                let images = protocol_images_to_attachments(payload.images);
                                let handle = handle.clone();
                                let command_error_tx = command_error_tx.clone();
                                tokio::spawn(async move {
                                    if let Err(err) = handle
                                        .execute(SessionCommand::SendMessage {
                                            message: payload.message,
                                            images,
                                        })
                                        .await
                                    {
                                        let _ = command_error_tx.send(format!(
                                            "Failed to send resumed Kiro follow-up prompt: {err}"
                                        ));
                                    }
                                });
                            }
                            AgentInput::UpdateSessionSettings(payload) => {
                                if let Err(err) = handle
                                    .execute(SessionCommand::UpdateSettings {
                                        settings: session_settings_to_json(&payload.values),
                                        persist: false,
                                    })
                                    .await
                                {
                                    tracing::error!("Failed to update resumed Kiro session settings: {err}");
                                    break;
                                }
                            }
                            AgentInput::EditQueuedMessage(_)
                            | AgentInput::CancelQueuedMessage(_)
                            | AgentInput::SendQueuedMessageNow(_) => {
                                panic!(
                                    "queued-message inputs must be handled by the agent actor before reaching the backend"
                                );
                            }
                        }
                    }
                    interrupt = interrupt_rx.recv() => {
                        let Some(()) = interrupt else { break };
                        if let Err(err) = handle.execute(SessionCommand::CancelConversation).await {
                            tracing::error!("Failed to interrupt resumed Kiro turn: {err}");
                            break;
                        }
                    }
                }
            }

            session.shutdown().await;
            let _ = forward_task.await;
        });

        Ok((
            Self {
                input_tx,
                interrupt_tx,
                events_tx: events_tx.clone(),
                session_id: known_session_id,
            },
            EventStream::new(events_rx),
        ))
    }

    async fn list_sessions() -> Result<Vec<BackendSession>, String> {
        let raw_sessions = load_local_kiro_sessions().await?;
        let mut sessions = Vec::new();
        for (session_id, metadata) in raw_sessions {
            let cwd = metadata
                .get("cwd")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if cwd.contains(KIRO_ADMIN_SESSION_SUBDIR)
                || cwd.contains(KIRO_EPHEMERAL_SESSION_SUBDIR)
            {
                continue;
            }
            sessions.push(BackendSession {
                id: SessionId(session_id),
                backend_kind: BackendKind::Kiro,
                workspace_roots: if cwd.is_empty() {
                    Vec::new()
                } else {
                    vec![cwd]
                },
                title: Some(extract_session_title(&metadata)),
                token_count: None,
                created_at_ms: Some(extract_session_timestamp(&metadata)),
                updated_at_ms: Some(extract_session_timestamp(&metadata)),
                resumable: true,
            });
        }
        sessions.sort_by(|a, b| b.updated_at_ms.cmp(&a.updated_at_ms));
        Ok(sessions)
    }

    async fn send(&self, input: AgentInput) -> bool {
        match input {
            AgentInput::SendMessage(payload) => {
                let visible_message = payload.message.clone();
                let visible_images = payload.images.clone();
                let outbound = AgentInput::SendMessage(protocol::SendMessagePayload {
                    message: payload.message,
                    images: payload.images,
                });

                if self
                    .events_tx
                    .send(backend_user_message(visible_message, visible_images))
                    .await
                    .is_err()
                {
                    return false;
                }

                self.input_tx.send(outbound).await.is_ok()
            }
            input @ AgentInput::UpdateSessionSettings(_) => self.input_tx.send(input).await.is_ok(),
            AgentInput::EditQueuedMessage(_)
            | AgentInput::CancelQueuedMessage(_)
            | AgentInput::SendQueuedMessageNow(_) => {
                panic!(
                    "queued-message inputs must be handled by the agent actor before reaching the backend"
                );
            }
        }
    }

    async fn interrupt(&self) -> bool {
        self.interrupt_tx.send(()).await.is_ok()
    }

    async fn shutdown(self) {
        drop(self);
    }

    fn session_id(&self) -> SessionId {
        self.session_id
            .lock()
            .expect("kiro session_id mutex poisoned")
            .clone()
            .expect("kiro session_id not initialized")
    }
}

fn map_kiro_value_to_chat_event(value: &Value) -> Option<ChatEvent> {
    if let Ok(event) = serde_json::from_value::<ChatEvent>(value.clone()) {
        return Some(event);
    }

    let kind = value
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or_default();

    match kind {
        "StreamStart" => {
            let data = value.get("data").unwrap_or(&Value::Null);
            Some(ChatEvent::StreamStart(StreamStartData {
                message_id: data
                    .get("message_id")
                    .or_else(|| data.get("messageId"))
                    .and_then(Value::as_str)
                    .map(|s| s.to_string()),
                agent: data
                    .get("agent")
                    .and_then(Value::as_str)
                    .unwrap_or(BACKEND_AGENT_NAME)
                    .to_string(),
                model: data
                    .get("model")
                    .and_then(Value::as_str)
                    .map(|s| s.to_string()),
            }))
        }
        "StreamDelta" => {
            let data = value.get("data").unwrap_or(&Value::Null);
            let text = data
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if text.is_empty() {
                return None;
            }
            Some(ChatEvent::StreamDelta(StreamTextDeltaData {
                message_id: data
                    .get("message_id")
                    .or_else(|| data.get("messageId"))
                    .and_then(Value::as_str)
                    .map(|s| s.to_string()),
                text,
            }))
        }
        "StreamEnd" => {
            let data = value.get("data").unwrap_or(&Value::Null);
            let msg = data.get("message").unwrap_or(&Value::Null);
            let content = msg
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let model = msg
                .get("model_info")
                .or_else(|| msg.get("modelInfo"))
                .and_then(|v| v.get("model"))
                .and_then(Value::as_str)
                .map(|s| s.to_string());
            Some(ChatEvent::StreamEnd(StreamEndData {
                message: ChatMessage {
                    timestamp: msg
                        .get("timestamp")
                        .and_then(Value::as_u64)
                        .unwrap_or_else(unix_now_ms),
                    sender: MessageSender::Assistant {
                        agent: BACKEND_AGENT_NAME.to_string(),
                    },
                    content,
                    reasoning: None,
                    tool_calls: Vec::new(),
                    model_info: model.map(|m| ModelInfo { model: m }),
                    token_usage: None,
                    context_breakdown: None,
                    images: None,
                },
            }))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;
    use std::time::Duration;

    #[test]
    fn extract_known_models_dedupes_case_variants() {
        let models = extract_known_models(&json!({
            "models": {
                "availableModels": [
                    { "id": "Auto", "name": "Auto", "isDefault": false },
                    { "id": "auto", "name": "Auto", "isDefault": true },
                    { "id": "claude-sonnet", "name": "Claude Sonnet", "isDefault": false }
                ]
            }
        }));

        assert_eq!(models.len(), 2);
        assert_eq!(models[0]["id"], Value::String("auto".to_string()));
        assert_eq!(models[0]["isDefault"], Value::Bool(true));
        assert_eq!(models[1]["id"], Value::String("claude-sonnet".to_string()));
    }

    // Real Kiro ACP events captured from a live session.

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

    fn write_fake_kiro_acp_program() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tyde-kiro-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create fake kiro tempdir");
        let path = dir.join("fake-kiro-acp");
        let script = r#"#!/bin/sh
read _
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1}}'
read _
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"sessionId":"kiro-test-session","models":{"currentModelId":"auto"}}}'
read _
printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"kiro-test-session","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"FAST_TURN_OK"}}}}'
printf '%s\n' '{"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn"}}'
"#;
        std::fs::write(&path, script).expect("write fake kiro acp program");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path)
                .expect("stat fake kiro acp program")
                .permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).expect("chmod fake kiro acp program");
        }
        path
    }

    #[tokio::test]
    async fn send_message_waits_for_prior_inbound_updates_before_finalizing_stream() {
        let workspace_root =
            std::env::temp_dir().join(format!("tyde-kiro-workspace-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&workspace_root).expect("create fake workspace");
        let program = write_fake_kiro_acp_program();

        let (session, mut raw_events) = KiroSession::spawn_admin_with_program_override(
            &[workspace_root.to_string_lossy().to_string()],
            None,
            None,
            &[],
            None,
            Some(program.to_string_lossy().to_string()),
        )
        .await
        .expect("spawn fake kiro session");

        let handle = session.command_handle();
        handle
            .execute(SessionCommand::SendMessage {
                message: "hello".to_string(),
                images: None,
            })
            .await
            .expect("send fake kiro message");

        let mut events = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(25), raw_events.recv()).await {
                Ok(Some(value)) => {
                    let kind = value
                        .get("kind")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let is_typing_false = kind == "TypingStatusChanged"
                        && value.get("data").and_then(Value::as_bool) == Some(false);
                    events.push(value);
                    if is_typing_false {
                        break;
                    }
                }
                Ok(None) => break,
                Err(_) => break,
            }
        }

        let first_typing_true = events.iter().position(|event| {
            event.get("kind").and_then(Value::as_str) == Some("TypingStatusChanged")
                && event.get("data").and_then(Value::as_bool) == Some(true)
        });
        let stream_start = events
            .iter()
            .position(|event| event.get("kind").and_then(Value::as_str) == Some("StreamStart"));
        let stream_delta = events
            .iter()
            .position(|event| event.get("kind").and_then(Value::as_str) == Some("StreamDelta"));
        let stream_end = events
            .iter()
            .position(|event| event.get("kind").and_then(Value::as_str) == Some("StreamEnd"));
        let typing_false = events.iter().position(|event| {
            event.get("kind").and_then(Value::as_str) == Some("TypingStatusChanged")
                && event.get("data").and_then(Value::as_bool) == Some(false)
        });

        assert!(
            first_typing_true.is_some()
                && stream_start.is_some()
                && stream_delta.is_some()
                && stream_end.is_some()
                && typing_false.is_some(),
            "expected full stream lifecycle after prompt completion, got {events:?}"
        );
        let first_typing_true = first_typing_true.expect("typing true checked above");
        let stream_start = stream_start.expect("stream start checked above");
        let stream_delta = stream_delta.expect("stream delta checked above");
        let stream_end = stream_end.expect("stream end checked above");
        let typing_false = typing_false.expect("typing false checked above");
        assert!(
            first_typing_true < stream_start
                && stream_start < stream_delta
                && stream_delta < stream_end
                && stream_end < typing_false,
            "expected ordered stream lifecycle after prompt completion, got {events:?}"
        );
        assert!(
            !events[..stream_end].iter().any(|event| {
                event.get("kind").and_then(Value::as_str) == Some("TypingStatusChanged")
                    && event.get("data").and_then(Value::as_bool) == Some(false)
            }),
            "saw TypingStatusChanged(false) before StreamEnd: {events:?}"
        );

        session.shutdown().await;
    }
}
