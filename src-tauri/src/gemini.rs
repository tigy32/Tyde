use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::process::ExitStatus;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{from_str, json, to_value, Map, Value};
use tokio::io::AsyncBufReadExt;
use tokio::io::BufReader;
use tokio::process::{ChildStderr, ChildStdout};
use tokio::sync::{mpsc, oneshot, Mutex};

use crate::backend::{SessionCommand, StartupMcpServer, StartupMcpTransport};
use crate::backend_transport::BackendTransport;
use crate::remote::{parse_remote_workspace_roots, shell_quote_arg, shell_quote_command};
use crate::subprocess::ImageAttachment;

const GEMINI_AGENT_NAME: &str = "gemini";
static GEMINI_TURN_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Clone)]
pub struct GeminiCommandHandle {
    inner: Arc<GeminiInner>,
}

impl GeminiCommandHandle {
    pub async fn execute(&self, command: SessionCommand) -> Result<(), String> {
        GeminiInner::execute_arc(Arc::clone(&self.inner), command).await
    }
}

#[derive(Clone)]
pub struct GeminiSession {
    inner: Arc<GeminiInner>,
}

impl GeminiSession {
    pub async fn spawn(
        workspace_roots: &[String],
        transport: BackendTransport,
        startup_mcp_servers: &[StartupMcpServer],
        steering_content: Option<&str>,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Value>), String> {
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
    ) -> Result<(Self, mpsc::UnboundedReceiver<Value>), String> {
        Self::spawn_with_mode(
            workspace_roots,
            true,
            transport,
            startup_mcp_servers,
            steering_content,
        )
        .await
    }

    pub async fn spawn_admin(
        workspace_roots: &[String],
        transport: BackendTransport,
        startup_mcp_servers: &[StartupMcpServer],
        steering_content: Option<&str>,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Value>), String> {
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
    ) -> Result<(Self, mpsc::UnboundedReceiver<Value>), String> {
        let workspace_root = if transport.is_remote() {
            let parsed = parse_remote_workspace_roots(workspace_roots)?
                .ok_or("Expected remote workspace roots for SSH session")?;
            parsed
                .1
                .into_iter()
                .next()
                .ok_or("No remote workspace root found")?
        } else {
            pick_workspace_root(workspace_roots)?
        };

        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let inner = Arc::new(GeminiInner {
            event_tx,
            state: Mutex::new(GeminiState {
                workspace_root,
                transport,
                ephemeral,
                session_id: None,
                model: None,
                permission_mode: None,
                steering_content: steering_content
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string()),
                startup_mcp_servers: startup_mcp_servers.to_vec(),
                active_turn: None,
            }),
        });

        Ok((Self { inner }, event_rx))
    }

    pub fn command_handle(&self) -> GeminiCommandHandle {
        GeminiCommandHandle {
            inner: Arc::clone(&self.inner),
        }
    }

    pub async fn shutdown(self) {
        self.inner.cancel_active_turn().await;
    }
}

struct ActiveTurn {
    id: u64,
    cancel_tx: Option<oneshot::Sender<()>>,
}

struct GeminiState {
    workspace_root: String,
    transport: BackendTransport,
    ephemeral: bool,
    session_id: Option<String>,
    model: Option<String>,
    permission_mode: Option<String>,
    steering_content: Option<String>,
    startup_mcp_servers: Vec<StartupMcpServer>,
    active_turn: Option<ActiveTurn>,
}

struct GeminiInner {
    event_tx: mpsc::UnboundedSender<Value>,
    state: Mutex<GeminiState>,
}

#[derive(Default)]
struct GeminiStdoutSummary {
    streamed_text: String,
    streamed_reasoning: String,
    session_id: Option<String>,
    model: Option<String>,
    usage: Option<Value>,
    errors: Vec<String>,
    tool_calls: Vec<GeminiToolCall>,
    seen_tool_ids: HashSet<String>,
    tool_name_by_id: HashMap<String, String>,
}

impl GeminiStdoutSummary {
    fn error_message(&self) -> Option<String> {
        self.errors
            .iter()
            .map(|msg| msg.trim())
            .find(|msg| !msg.is_empty())
            .map(|msg| msg.to_string())
    }
}

#[derive(Clone)]
struct GeminiToolCall {
    id: String,
    name: String,
    arguments: Value,
}

#[derive(Default)]
struct SegmentState {
    has_content: bool,
    segment_index: u64,
    awaiting_stream_start: bool,
}

enum WaitResult {
    Exited(Result<ExitStatus, String>),
    Cancelled,
}

enum TurnOutcome {
    Completed {
        summary: GeminiStdoutSummary,
    },
    Cancelled {
        summary: GeminiStdoutSummary,
    },
    Failed {
        summary: GeminiStdoutSummary,
        error: String,
    },
}

// ---------------------------------------------------------------------------
// GeminiInner — command execution and turn lifecycle
// ---------------------------------------------------------------------------

impl GeminiInner {
    async fn execute_arc(this: Arc<Self>, command: SessionCommand) -> Result<(), String> {
        match command {
            SessionCommand::SendMessage { message, images } => {
                this.emit_user_message_added(&message, images.as_deref());
                this.start_turn(message, images).await;
                Ok(())
            }
            SessionCommand::CancelConversation => {
                this.cancel_active_turn().await;
                Ok(())
            }
            SessionCommand::GetSettings => {
                this.emit_settings().await;
                Ok(())
            }
            SessionCommand::ListModels => {
                this.emit_event(json!({
                    "kind": "ModelsList",
                    "data": { "models": gemini_known_models() }
                }));
                Ok(())
            }
            SessionCommand::UpdateSettings {
                settings,
                persist: _,
            } => {
                if let Some(obj) = settings.as_object() {
                    let mut state = this.state.lock().await;
                    if let Some(model_value) = obj.get("model") {
                        state.model = normalize_optional_string(model_value);
                    }
                    if let Some(pm_value) = obj
                        .get("permission_mode")
                        .or_else(|| obj.get("permissionMode"))
                    {
                        state.permission_mode = normalize_optional_string(pm_value);
                    }
                }
                this.emit_settings().await;
                Ok(())
            }
            SessionCommand::ListSessions => this.list_sessions().await,
            SessionCommand::ResumeSession { session_id } => this.resume_session(session_id).await,
            SessionCommand::DeleteSession { session_id } => this.delete_session(session_id).await,
            SessionCommand::ListProfiles => {
                this.emit_event(json!({
                    "kind": "ProfilesList",
                    "data": { "profiles": [] }
                }));
                Ok(())
            }
            SessionCommand::SwitchProfile { profile_name: _ } => Ok(()),
            SessionCommand::GetModuleSchemas => {
                this.emit_event(json!({
                    "kind": "ModuleSchemas",
                    "data": { "schemas": [] }
                }));
                Ok(())
            }
        }
    }

    async fn start_turn(self: Arc<Self>, message: String, images: Option<Vec<ImageAttachment>>) {
        if images.as_ref().is_some_and(|imgs| !imgs.is_empty()) {
            self.emit_error("Gemini CLI does not support image input in headless mode.");
            return;
        }

        let (
            turn_id,
            workspace_root,
            transport,
            ephemeral,
            session_id,
            model,
            permission_mode,
            steering_content,
            startup_mcp_servers,
            cancel_rx,
        ) = {
            let mut state = self.state.lock().await;
            if state.active_turn.is_some() {
                self.emit_error("Gemini is still processing the previous turn.");
                return;
            }

            let turn_id = GEMINI_TURN_COUNTER.fetch_add(1, Ordering::Relaxed);
            let (cancel_tx, cancel_rx) = oneshot::channel();
            state.active_turn = Some(ActiveTurn {
                id: turn_id,
                cancel_tx: Some(cancel_tx),
            });

            (
                turn_id,
                state.workspace_root.clone(),
                state.transport.clone(),
                state.ephemeral,
                state.session_id.clone(),
                state.model.clone(),
                state.permission_mode.clone(),
                state.steering_content.clone(),
                state.startup_mcp_servers.clone(),
                cancel_rx,
            )
        };

        let message_id = format!("gemini-msg-{turn_id}");
        self.emit_typing_status(true);
        self.emit_stream_start(&message_id, model.clone());

        tokio::spawn(async move {
            let outcome = self
                .run_turn(
                    &message_id,
                    &workspace_root,
                    &transport,
                    &message,
                    session_id,
                    model,
                    permission_mode.as_deref(),
                    steering_content.as_deref(),
                    &startup_mcp_servers,
                    cancel_rx,
                )
                .await;

            match outcome {
                TurnOutcome::Completed { mut summary } => {
                    if let Some(session_id) = summary.session_id.clone() {
                        self.set_session_id(session_id.clone()).await;
                        if !ephemeral {
                            self.emit_event(json!({
                                "kind": "SessionStarted",
                                "data": { "session_id": session_id }
                            }));
                        }
                    }
                    if !self.emit_summary_and_tool_requests(&mut summary) {
                        let error = summary
                            .error_message()
                            .unwrap_or_else(|| "Gemini returned no assistant output.".to_string());
                        self.emit_error(&error);
                    }
                }
                TurnOutcome::Cancelled { mut summary } => {
                    if let Some(session_id) = summary.session_id.clone() {
                        self.set_session_id(session_id.clone()).await;
                        if !ephemeral {
                            self.emit_event(json!({
                                "kind": "SessionStarted",
                                "data": { "session_id": session_id }
                            }));
                        }
                    }
                    if !self.emit_summary_and_tool_requests(&mut summary) {
                        self.emit_error("Gemini turn cancelled.");
                    }
                    self.emit_operation_cancelled("Gemini turn cancelled.");
                }
                TurnOutcome::Failed { summary, error } => {
                    let detail = summary.error_message().unwrap_or(error);
                    self.emit_error(&detail);
                }
            }

            self.clear_active_turn(turn_id).await;
            self.emit_typing_status(false);
        });
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_turn(
        self: &Arc<Self>,
        message_id: &str,
        workspace_root: &str,
        transport: &BackendTransport,
        prompt: &str,
        session_id: Option<String>,
        model: Option<String>,
        _permission_mode: Option<&str>,
        steering_content: Option<&str>,
        startup_mcp_servers: &[StartupMcpServer],
        cancel_rx: oneshot::Receiver<()>,
    ) -> TurnOutcome {
        let effective_prompt = match steering_content.filter(|s| !s.trim().is_empty()) {
            Some(steering) => format!("{steering}\n\n{prompt}"),
            None => prompt.to_string(),
        };

        let mut cli_args: Vec<String> = vec![
            "-y".to_string(),
            "-p".to_string(),
            effective_prompt,
            "--output-format".to_string(),
            "stream-json".to_string(),
        ];

        if let Some(existing_session) = session_id {
            let trimmed = existing_session.trim();
            if !trimmed.is_empty() {
                cli_args.push("--resume".to_string());
                cli_args.push(trimmed.to_string());
            }
        }

        if let Some(model_name) = model.as_deref().filter(|m| !m.trim().is_empty()) {
            cli_args.push("--model".to_string());
            cli_args.push(model_name.to_string());
        }

        // Gemini CLI reads MCP config from {workspace_root}/.gemini/settings.json.
        // Inject startup MCP servers by writing that file, restoring original after.
        let mcp_settings_json = build_gemini_settings_json(startup_mcp_servers);
        let mut mcp_cleanup: Option<GeminiMcpCleanup> = None;

        if let Some(ref json) = mcp_settings_json {
            if !transport.is_remote() {
                match inject_gemini_mcp_settings(workspace_root, json) {
                    Ok(cleanup) => mcp_cleanup = Some(cleanup),
                    Err(err) => {
                        return TurnOutcome::Failed {
                            summary: GeminiStdoutSummary::default(),
                            error: format!("Failed to write Gemini MCP settings: {err}"),
                        };
                    }
                }
            }
        }

        let (program, args, cwd) = if transport.is_remote() {
            let quoted_args = shell_quote_command(&cli_args);
            // For remote: write .gemini/settings.json on the remote host via heredoc,
            // run gemini, then restore the original file.
            let remote_cmd = if let Some(ref json) = mcp_settings_json {
                let settings_path = format!("{}/.gemini/settings.json", workspace_root);
                let backup_path = format!("{}/.gemini/settings.json.tyde-backup", workspace_root);
                format!(
                    "mkdir -p {}/.gemini && \
                     {{ [ -f {settings} ] && cp {settings} {backup}; }} 2>/dev/null; \
                     cat > {settings} <<'TYDE_MCP_EOF'\n{json}\nTYDE_MCP_EOF\n\
                     cd {ws} && PATH=\"$HOME/.cargo/bin:$HOME/.local/bin:/usr/local/bin:$PATH\" gemini {args}; \
                     _exit=$?; \
                     {{ [ -f {backup} ] && mv {backup} {settings} || rm -f {settings}; }} 2>/dev/null; \
                     exit $_exit",
                    shell_quote_arg(workspace_root),
                    settings = shell_quote_arg(&settings_path),
                    backup = shell_quote_arg(&backup_path),
                    ws = shell_quote_arg(workspace_root),
                    args = quoted_args,
                )
            } else {
                format!(
                    "cd {} && PATH=\"$HOME/.cargo/bin:$HOME/.local/bin:/usr/local/bin:$PATH\" gemini {}",
                    shell_quote_arg(workspace_root),
                    quoted_args,
                )
            };
            ("sh".to_string(), vec!["-lc".to_string(), remote_cmd], None)
        } else {
            ("gemini".to_string(), cli_args.clone(), Some(workspace_root))
        };

        let mut child = match transport.spawn_process(&program, &args, cwd).await {
            Ok(child) => child,
            Err(err) => {
                restore_gemini_mcp_settings(mcp_cleanup.take());
                return TurnOutcome::Failed {
                    summary: GeminiStdoutSummary::default(),
                    error: format!("Failed to start Gemini CLI: {err}"),
                };
            }
        };

        let stdout = match child.stdout.take() {
            Some(stdout) => stdout,
            None => {
                restore_gemini_mcp_settings(mcp_cleanup.take());
                return TurnOutcome::Failed {
                    summary: GeminiStdoutSummary::default(),
                    error: "Failed to capture Gemini stdout".to_string(),
                };
            }
        };

        let stderr = match child.stderr.take() {
            Some(stderr) => stderr,
            None => {
                restore_gemini_mcp_settings(mcp_cleanup.take());
                return TurnOutcome::Failed {
                    summary: GeminiStdoutSummary::default(),
                    error: "Failed to capture Gemini stderr".to_string(),
                };
            }
        };

        let stdout_task = tokio::spawn(read_gemini_stdout(
            stdout,
            Arc::clone(self),
            message_id.to_string(),
        ));
        let stderr_task = tokio::spawn(read_gemini_stderr(stderr));

        let mut cancel_rx = cancel_rx;
        let wait_result = tokio::select! {
            _ = &mut cancel_rx => WaitResult::Cancelled,
            status = child.wait() => {
                WaitResult::Exited(status.map_err(|err| format!("Failed to wait for Gemini process: {err:?}")))
            }
        };

        if matches!(wait_result, WaitResult::Cancelled) {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }

        let stdout_summary = match stdout_task.await {
            Ok(summary) => summary,
            Err(err) => {
                restore_gemini_mcp_settings(mcp_cleanup.take());
                return TurnOutcome::Failed {
                    summary: GeminiStdoutSummary::default(),
                    error: format!("Failed to collect Gemini stdout: {err:?}"),
                };
            }
        };

        let stderr_output = match stderr_task.await {
            Ok(stderr) => stderr,
            Err(err) => {
                restore_gemini_mcp_settings(mcp_cleanup.take());
                return TurnOutcome::Failed {
                    summary: stdout_summary,
                    error: format!("Failed to collect Gemini stderr: {err:?}"),
                };
            }
        };

        restore_gemini_mcp_settings(mcp_cleanup.take());

        match wait_result {
            WaitResult::Cancelled => TurnOutcome::Cancelled {
                summary: stdout_summary,
            },
            WaitResult::Exited(Err(error)) => TurnOutcome::Failed {
                summary: stdout_summary,
                error,
            },
            WaitResult::Exited(Ok(status)) => {
                evaluate_exit_status(status, stdout_summary, &stderr_output)
            }
        }
    }

    async fn cancel_active_turn(&self) {
        let cancel_tx = {
            let mut state = self.state.lock().await;
            state
                .active_turn
                .as_mut()
                .and_then(|active| active.cancel_tx.take())
        };
        if let Some(cancel_tx) = cancel_tx {
            let _ = cancel_tx.send(());
        }
    }

    async fn clear_active_turn(&self, turn_id: u64) {
        let mut state = self.state.lock().await;
        if state
            .active_turn
            .as_ref()
            .is_some_and(|active| active.id == turn_id)
        {
            state.active_turn = None;
        }
    }

    async fn set_session_id(&self, session_id: String) {
        let mut state = self.state.lock().await;
        state.session_id = Some(session_id);
    }

    async fn list_sessions(&self) -> Result<(), String> {
        let (workspace_root, transport) = {
            let state = self.state.lock().await;
            (state.workspace_root.clone(), state.transport.clone())
        };

        let entries = list_gemini_sessions_for_transport(&transport, &workspace_root).await?;
        let sessions: Vec<Value> = entries
            .iter()
            .map(|entry| {
                json!({
                    "id": &entry.session_id,
                    "session_id": &entry.session_id,
                    "title": &entry.title,
                    "created_at": entry.last_modified_ms,
                    "last_modified": entry.last_modified_ms,
                    "last_message_preview": &entry.preview,
                    "workspace_root": &workspace_root,
                    "message_count": Value::Null,
                    "backend_kind": "gemini",
                })
            })
            .collect();

        self.emit_event(json!({
            "kind": "SessionsList",
            "data": { "sessions": sessions }
        }));
        Ok(())
    }

    async fn resume_session(&self, session_id: String) -> Result<(), String> {
        let normalized = normalize_nonempty(&session_id).ok_or("Invalid session id")?;

        {
            let mut state = self.state.lock().await;
            state.session_id = Some(normalized.clone());
        }

        self.emit_event(json!({ "kind": "ConversationCleared" }));
        self.emit_event(json!({
            "kind": "SessionStarted",
            "data": { "session_id": normalized }
        }));
        Ok(())
    }

    async fn delete_session(&self, session_id: String) -> Result<(), String> {
        let normalized = normalize_nonempty(&session_id).ok_or("Invalid session id")?;
        let (workspace_root, transport) = {
            let state = self.state.lock().await;
            (state.workspace_root.clone(), state.transport.clone())
        };

        let entries = list_gemini_sessions_for_transport(&transport, &workspace_root).await?;
        let entry = entries
            .iter()
            .find(|candidate| candidate.session_id.as_str() == normalized.as_str())
            .ok_or_else(|| format!("Gemini session not found: {normalized}"))?;

        delete_gemini_session_for_transport(&transport, &workspace_root, &entry.index).await?;

        {
            let mut state = self.state.lock().await;
            if state.session_id.as_deref() == Some(normalized.as_str()) {
                state.session_id = None;
            }
        }

        self.list_sessions().await
    }

    async fn emit_settings(&self) {
        let (model, permission_mode, session_id) = {
            let state = self.state.lock().await;
            (
                state.model.clone(),
                state.permission_mode.clone(),
                state.session_id.clone(),
            )
        };
        self.emit_event(json!({
            "kind": "Settings",
            "data": {
                "model": model,
                "permission_mode": permission_mode,
                "session_id": session_id,
            }
        }));
    }

    // -----------------------------------------------------------------------
    // Event emission helpers
    // -----------------------------------------------------------------------

    fn emit_summary_and_tool_requests(&self, summary: &mut GeminiStdoutSummary) -> bool {
        let text = summary.streamed_text.trim().to_string();
        let reasoning = if summary.streamed_reasoning.trim().is_empty() {
            None
        } else {
            Some(summary.streamed_reasoning.trim().to_string())
        };
        let tool_calls_json: Vec<Value> = summary
            .tool_calls
            .iter()
            .map(|tc| json!({ "id": tc.id, "name": tc.name, "arguments": tc.arguments }))
            .collect();

        let has_output = !text.is_empty() || reasoning.is_some() || !tool_calls_json.is_empty();
        if !has_output {
            return false;
        }

        self.emit_stream_end(
            text,
            summary.model.clone(),
            summary.usage.take(),
            reasoning,
            tool_calls_json,
        );
        for tool_call in &summary.tool_calls {
            self.emit_tool_request(tool_call);
        }
        true
    }

    fn emit_event(&self, event: Value) {
        if let Err(e) = self.event_tx.send(event) {
            tracing::trace!("event send failed: {e}");
        }
    }

    fn emit_typing_status(&self, typing: bool) {
        self.emit_event(json!({
            "kind": "TypingStatusChanged",
            "data": typing,
        }));
    }

    fn emit_stream_start(&self, message_id: &str, model: Option<String>) {
        let model_value = model.map(Value::String).unwrap_or(Value::Null);
        self.emit_event(json!({
            "kind": "StreamStart",
            "data": {
                "message_id": message_id,
                "agent": GEMINI_AGENT_NAME,
                "model": model_value,
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

    fn emit_stream_delta(&self, message_id: &str, text: &str) {
        if text.is_empty() {
            return;
        }
        self.emit_event(json!({
            "kind": "StreamDelta",
            "data": {
                "message_id": message_id,
                "text": text,
            }
        }));
    }

    fn emit_stream_end(
        &self,
        content: String,
        model: Option<String>,
        usage: Option<Value>,
        reasoning: Option<String>,
        tool_calls: Vec<Value>,
    ) {
        let model_hint = model.clone();
        let model_info = model
            .filter(|m| !m.trim().is_empty())
            .map(|m| json!({ "model": m }))
            .unwrap_or(Value::Null);
        let usage_value = usage.unwrap_or(Value::Null);
        let context_breakdown_value =
            estimate_gemini_context_breakdown(Some(&usage_value), model_hint.as_deref())
                .unwrap_or(Value::Null);
        let reasoning_value = reasoning
            .filter(|v| !v.trim().is_empty())
            .map(|text| json!({ "text": text }))
            .unwrap_or(Value::Null);

        self.emit_event(json!({
            "kind": "StreamEnd",
            "data": {
                "message": {
                    "timestamp": unix_now_ms(),
                    "sender": { "Assistant": { "agent": GEMINI_AGENT_NAME } },
                    "content": content,
                    "reasoning": reasoning_value,
                    "tool_calls": tool_calls,
                    "model_info": model_info,
                    "token_usage": usage_value,
                    "context_breakdown": context_breakdown_value,
                    "images": [],
                }
            }
        }));
    }

    fn emit_operation_cancelled(&self, message: &str) {
        self.emit_event(json!({
            "kind": "OperationCancelled",
            "data": {
                "message": message,
            }
        }));
    }

    fn emit_error(&self, message: &str) {
        self.emit_event(json!({
            "kind": "Error",
            "data": message,
        }));
    }

    fn emit_tool_request(&self, tool_call: &GeminiToolCall) {
        self.emit_event(json!({
            "kind": "ToolRequest",
            "data": {
                "tool_call_id": tool_call.id,
                "tool_name": tool_call.name,
                "tool_type": gemini_tool_request_type(&tool_call.name, &tool_call.arguments),
            }
        }));
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
                "error": error,
            }
        }));
    }
}

// ---------------------------------------------------------------------------
// Stdout / stderr readers
// ---------------------------------------------------------------------------

async fn read_gemini_stdout(
    stdout: ChildStdout,
    inner: Arc<GeminiInner>,
    base_message_id: String,
) -> GeminiStdoutSummary {
    let mut summary = GeminiStdoutSummary::default();
    let mut segment = SegmentState::default();
    let mut current_message_id = base_message_id.clone();
    let mut lines = BufReader::new(stdout).lines();

    while let Ok(Some(line)) = lines.next_line().await {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let value = match from_str::<Value>(trimmed) {
            Ok(value) => value,
            Err(_) => {
                tracing::warn!("Non-JSON line from Gemini CLI: {trimmed}");
                continue;
            }
        };

        consume_gemini_event(
            &value,
            &mut summary,
            &mut segment,
            &inner,
            &base_message_id,
            &mut current_message_id,
        );
    }

    summary
}

async fn read_gemini_stderr(stderr: ChildStderr) -> String {
    let mut out = String::new();
    let mut lines = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&line);
    }
    out
}

// ---------------------------------------------------------------------------
// NDJSON event consumption
// ---------------------------------------------------------------------------

fn consume_gemini_event(
    value: &Value,
    summary: &mut GeminiStdoutSummary,
    segment: &mut SegmentState,
    inner: &GeminiInner,
    base_message_id: &str,
    current_message_id: &mut String,
) {
    let event_type = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();

    match event_type {
        "init" => {
            if let Some(model) = value.get("model").and_then(Value::as_str) {
                summary.model = Some(model.to_string());
            }
            if let Some(session_id) = value
                .get("session_id")
                .or_else(|| value.get("sessionId"))
                .and_then(Value::as_str)
            {
                let normalized = session_id.trim();
                if !normalized.is_empty() {
                    summary.session_id = Some(normalized.to_string());
                }
            }
        }
        "message" => {
            consume_message_event(
                value,
                summary,
                segment,
                inner,
                base_message_id,
                current_message_id,
            );
        }
        "tool_use" => {
            let Some(tool_call) = extract_gemini_tool_call(value) else {
                return;
            };
            maybe_emit_next_stream_start(
                segment,
                inner,
                base_message_id,
                current_message_id,
                summary.model.clone(),
            );
            if !summary.seen_tool_ids.contains(&tool_call.id) {
                summary.seen_tool_ids.insert(tool_call.id.clone());
                summary
                    .tool_name_by_id
                    .insert(tool_call.id.clone(), tool_call.name.clone());
                summary.tool_calls.push(tool_call);
                segment.has_content = true;
            }
            close_current_phase(summary, segment, inner);
        }
        "tool_result" => {
            let tool_call_id = value
                .get("tool_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let tool_name = summary
                .tool_name_by_id
                .get(&tool_call_id)
                .cloned()
                .unwrap_or_else(|| "tool".to_string());
            let is_error = value
                .get("status")
                .and_then(Value::as_str)
                .is_some_and(|s| s == "error");
            let result_content = extract_tool_result_content(value);

            let tool_result = if is_error {
                json!({
                    "kind": "Error",
                    "short_message": result_content,
                    "detailed_message": result_content,
                })
            } else {
                map_tool_completion_result(&tool_name, &result_content)
            };

            let error = if is_error { Some(result_content) } else { None };
            inner.emit_tool_execution_completed(
                &tool_call_id,
                &tool_name,
                !is_error,
                tool_result,
                error,
            );

            if !segment.awaiting_stream_start {
                segment.awaiting_stream_start = true;
            }
        }
        "error" => {
            let message = value
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("Gemini error")
                .to_string();
            summary.errors.push(message);
        }
        "result" => {
            if let Some(usage) = value.get("stats").and_then(|v| parse_gemini_usage(Some(v))) {
                summary.usage = Some(usage);
            }
            if value
                .get("status")
                .and_then(Value::as_str)
                .is_some_and(|s| s == "error")
            {
                let error_msg = value
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("Gemini result error")
                    .to_string();
                summary.errors.push(error_msg);
            }
        }
        _ => {
            tracing::debug!("Unknown Gemini event type: {event_type}");
        }
    }
}

// ---------------------------------------------------------------------------
// Message text / reasoning extraction
// ---------------------------------------------------------------------------

fn consume_message_event(
    value: &Value,
    summary: &mut GeminiStdoutSummary,
    segment: &mut SegmentState,
    inner: &GeminiInner,
    base_message_id: &str,
    current_message_id: &mut String,
) {
    // Gemini CLI echoes the user's prompt as a message with role "user".
    // Only process assistant messages.
    let role = value.get("role").and_then(Value::as_str).unwrap_or("");
    if role == "user" {
        return;
    }

    let text = extract_message_text(value).filter(|t| !t.trim().is_empty());

    if let Some(ref text) = text {
        maybe_emit_next_stream_start(
            segment,
            inner,
            base_message_id,
            current_message_id,
            summary.model.clone(),
        );
        summary.streamed_text.push_str(text);
        segment.has_content = true;
        inner.emit_stream_delta(current_message_id, text);
    }
}

fn extract_message_text(value: &Value) -> Option<String> {
    value
        .get("content")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

fn extract_gemini_tool_call(value: &Value) -> Option<GeminiToolCall> {
    let id = value
        .get("tool_id")
        .and_then(Value::as_str)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())?;
    let name = value
        .get("tool_name")
        .and_then(Value::as_str)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "tool".to_string());
    let arguments = value.get("parameters").cloned().unwrap_or(Value::Null);

    Some(GeminiToolCall {
        id,
        name,
        arguments,
    })
}

fn extract_tool_result_content(value: &Value) -> String {
    if let Some(output) = value.get("output").and_then(Value::as_str) {
        return output.to_string();
    }
    if let Some(message) = value
        .get("error")
        .and_then(|e| e.get("message"))
        .and_then(Value::as_str)
    {
        return message.to_string();
    }
    String::new()
}

// ---------------------------------------------------------------------------
// Phase / segment management
// ---------------------------------------------------------------------------

fn maybe_emit_next_stream_start(
    segment: &mut SegmentState,
    inner: &GeminiInner,
    base_message_id: &str,
    current_message_id: &mut String,
    model: Option<String>,
) {
    if !segment.awaiting_stream_start {
        return;
    }
    segment.segment_index += 1;
    *current_message_id = format!("{base_message_id}-seg-{}", segment.segment_index);
    inner.emit_stream_start(current_message_id, model);
    segment.awaiting_stream_start = false;
}

fn close_current_phase(
    summary: &mut GeminiStdoutSummary,
    segment: &mut SegmentState,
    inner: &GeminiInner,
) {
    let text = summary.streamed_text.trim().to_string();
    let reasoning = if summary.streamed_reasoning.trim().is_empty() {
        None
    } else {
        Some(summary.streamed_reasoning.trim().to_string())
    };
    let tool_calls_json: Vec<Value> = summary
        .tool_calls
        .iter()
        .map(|tc| {
            json!({
                "id": tc.id,
                "name": tc.name,
                "arguments": tc.arguments,
            })
        })
        .collect();

    if !text.is_empty() || reasoning.is_some() || !tool_calls_json.is_empty() {
        inner.emit_stream_end(
            text,
            summary.model.clone(),
            summary.usage.clone(),
            reasoning,
            tool_calls_json,
        );
        for tool_call in &summary.tool_calls {
            inner.emit_tool_request(tool_call);
        }
    }

    summary.streamed_text.clear();
    summary.streamed_reasoning.clear();
    summary.tool_calls.clear();
    segment.has_content = false;
    segment.awaiting_stream_start = true;
}

// ---------------------------------------------------------------------------
// Tool type mapping
// ---------------------------------------------------------------------------

fn gemini_tool_request_type(tool_name: &str, arguments: &Value) -> Value {
    match tool_name {
        "replace" | "write_file" => {
            let file_path = gemini_argument_file_path(arguments);
            json!({
                "kind": "ModifyFile",
                "file_path": file_path,
                "before": "",
                "after": "",
            })
        }
        "run_shell_command" => {
            let command = arguments
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            json!({
                "kind": "RunCommand",
                "command": command,
                "working_directory": "",
            })
        }
        "read_file" | "read_many_files" | "list_directory" | "glob" | "grep_search" => {
            let file_paths = gemini_argument_file_paths(arguments);
            json!({
                "kind": "ReadFiles",
                "file_paths": file_paths,
            })
        }
        _ => json!({ "kind": "Other", "args": arguments }),
    }
}

fn gemini_argument_file_path(arguments: &Value) -> String {
    arguments
        .get("file_path")
        .and_then(Value::as_str)
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .unwrap_or("")
        .to_string()
}

fn gemini_argument_file_paths(arguments: &Value) -> Vec<String> {
    let single = gemini_argument_file_path(arguments);
    if !single.is_empty() {
        return vec![single];
    }
    if let Some(arr) = arguments.get("file_paths").and_then(Value::as_array) {
        let paths: Vec<String> = arr
            .iter()
            .filter_map(Value::as_str)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if !paths.is_empty() {
            return paths;
        }
    }
    Vec::new()
}

fn map_tool_completion_result(tool_name: &str, result_content: &str) -> Value {
    match tool_name {
        "replace" | "write_file" => {
            json!({ "kind": "ModifyFile", "lines_added": 0, "lines_removed": 0 })
        }
        "run_shell_command" => {
            json!({
                "kind": "RunCommand",
                "exit_code": 0,
                "stdout": result_content,
                "stderr": "",
            })
        }
        "read_file" | "read_many_files" | "list_directory" | "glob" | "grep_search" => {
            json!({ "kind": "ReadFiles", "files": [] })
        }
        _ => json!({ "kind": "Other", "result": result_content }),
    }
}

// ---------------------------------------------------------------------------
// Known models
// ---------------------------------------------------------------------------

fn gemini_known_models() -> Vec<Value> {
    let models: &[(&str, &str, bool)] = &[
        ("auto-gemini-2.5", "Auto (Gemini 2.5)", true),
        ("auto-gemini-3", "Auto (Gemini 3)", false),
        ("gemini-3.1-pro-preview", "Gemini 3.1 Pro Preview", false),
        ("gemini-3-pro-preview", "Gemini 3 Pro Preview", false),
        ("gemini-3-flash-preview", "Gemini 3 Flash Preview", false),
        (
            "gemini-3.1-flash-lite-preview",
            "Gemini 3.1 Flash Lite Preview",
            false,
        ),
        ("gemini-2.5-pro", "Gemini 2.5 Pro", false),
        ("gemini-2.5-flash", "Gemini 2.5 Flash", false),
        ("gemini-2.5-flash-lite", "Gemini 2.5 Flash Lite", false),
    ];
    models
        .iter()
        .map(|(id, display, default)| {
            json!({ "id": id, "displayName": display, "isDefault": default })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Usage parsing
// ---------------------------------------------------------------------------

fn parse_gemini_usage(raw: Option<&Value>) -> Option<Value> {
    let stats = raw?.as_object()?;

    let raw_input_tokens = usage_u64(
        stats,
        &[
            "input_tokens",
            "inputTokens",
            "prompt_tokens",
            "promptTokens",
        ],
    )
    .unwrap_or(0);
    let cached_prompt_tokens = usage_u64(
        stats,
        &[
            "cached",
            "cached_tokens",
            "cached_prompt_tokens",
            "cache_read_input_tokens",
            "cacheReadInputTokens",
        ],
    )
    .unwrap_or(0);
    let cache_creation_input_tokens = usage_u64(
        stats,
        &[
            "cache_creation_input_tokens",
            "cacheCreationInputTokens",
            "cache_write_input_tokens",
            "cacheWriteInputTokens",
        ],
    )
    .unwrap_or(0);
    let input_tokens = usage_u64(
        stats,
        &[
            "input",
            "input_non_cached",
            "input_non_cached_tokens",
            "inputNonCachedTokens",
        ],
    )
    .unwrap_or_else(|| {
        raw_input_tokens
            .saturating_sub(cached_prompt_tokens)
            .saturating_sub(cache_creation_input_tokens)
    });
    let output_tokens = usage_u64(
        stats,
        &[
            "output_tokens",
            "outputTokens",
            "completion_tokens",
            "completionTokens",
        ],
    )
    .unwrap_or(0);
    let total_tokens = usage_u64(stats, &["total_tokens", "totalTokens", "total"])
        .unwrap_or(input_tokens.saturating_add(output_tokens));
    let reasoning_tokens = usage_u64(
        stats,
        &[
            "reasoning_tokens",
            "reasoningTokens",
            "thoughts_tokens",
            "thoughtsTokens",
        ],
    )
    .unwrap_or(0);
    let duration_ms = usage_u64(stats, &["duration_ms", "durationMs"]).unwrap_or(0);
    let tool_call_count = usage_u64(stats, &["tool_calls", "toolCalls"]).unwrap_or(0);
    let context_window = usage_u64(
        stats,
        &[
            "context_window",
            "contextWindow",
            "max_input_tokens",
            "maxInputTokens",
        ],
    );

    if input_tokens == 0
        && output_tokens == 0
        && total_tokens == 0
        && cached_prompt_tokens == 0
        && cache_creation_input_tokens == 0
    {
        return None;
    }

    let mut usage = json!({
        "input_tokens": input_tokens,
        "output_tokens": output_tokens,
        "total_tokens": total_tokens,
        "cached_prompt_tokens": cached_prompt_tokens,
        "cache_creation_input_tokens": cache_creation_input_tokens,
        "reasoning_tokens": reasoning_tokens,
        "duration_ms": duration_ms,
        "tool_calls": tool_call_count,
    });
    if let Some(window) = context_window {
        usage["context_window"] = json!(window);
    }
    Some(usage)
}

fn usage_u64(source: &Map<String, Value>, keys: &[&str]) -> Option<u64> {
    for key in keys {
        if let Some(value) = source.get(*key) {
            if let Some(num) = value.as_u64() {
                return Some(num);
            }
            if let Some(str_value) = value.as_str() {
                if let Ok(num) = str_value.trim().parse::<u64>() {
                    return Some(num);
                }
            }
        }
    }
    None
}

fn estimate_gemini_context_breakdown(
    token_usage: Option<&Value>,
    model_hint: Option<&str>,
) -> Option<Value> {
    let usage = token_usage?;
    let base_input_tokens = usage
        .get("input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cached_prompt_tokens = usage
        .get("cached_prompt_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cache_creation_input_tokens = usage
        .get("cache_creation_input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let reasoning_tokens = usage
        .get("reasoning_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);

    let input_tokens = base_input_tokens
        .saturating_add(cached_prompt_tokens)
        .saturating_add(cache_creation_input_tokens);
    if input_tokens == 0 {
        return None;
    }

    let context_window = usage
        .get("context_window")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| {
            std::cmp::max(
                gemini_estimated_context_window_for_model(model_hint),
                input_tokens.max(1),
            )
        });

    Some(json!({
        "system_prompt_bytes": 0,
        "tool_io_bytes": 0,
        "conversation_history_bytes": 0,
        "reasoning_bytes": reasoning_tokens.saturating_mul(4),
        "context_injection_bytes": input_tokens.saturating_mul(4),
        "input_tokens": input_tokens,
        "context_window": context_window,
    }))
}

fn gemini_estimated_context_window_for_model(model_hint: Option<&str>) -> u64 {
    let normalized = model_hint.unwrap_or_default().trim().to_ascii_lowercase();
    if normalized.contains("flash-lite") {
        return 1_048_576;
    }
    if normalized.contains("flash") {
        return 1_048_576;
    }
    if normalized.contains("pro") {
        return 1_048_576;
    }
    if normalized.contains("gemini-3") || normalized.contains("gemini-2.5") {
        return 1_048_576;
    }
    1_048_576
}

#[derive(Debug, Clone)]
struct GeminiSessionListEntry {
    index: String,
    session_id: String,
    title: String,
    preview: String,
    last_modified_ms: u64,
}

async fn list_gemini_sessions_for_transport(
    transport: &BackendTransport,
    workspace_root: &str,
) -> Result<Vec<GeminiSessionListEntry>, String> {
    let output =
        run_gemini_cli_command_output(transport, workspace_root, &["--list-sessions"]).await?;
    let text = combine_process_output(&output);
    let sessions = parse_gemini_session_entries(&text);

    if sessions.is_empty() && !output.status.success() {
        let detail = first_nonempty_line(&text).unwrap_or("Gemini --list-sessions failed");
        return Err(detail.to_string());
    }

    Ok(sessions)
}

async fn delete_gemini_session_for_transport(
    transport: &BackendTransport,
    workspace_root: &str,
    index: &str,
) -> Result<(), String> {
    let output =
        run_gemini_cli_command_output(transport, workspace_root, &["--delete-session", index])
            .await?;
    let text = combine_process_output(&output);
    let lowered = text.to_ascii_lowercase();
    if lowered.contains("invalid session identifier")
        || lowered.contains("session not found")
        || lowered.contains("unknown session")
    {
        let detail = first_nonempty_line(&text).unwrap_or("Gemini session not found");
        return Err(detail.to_string());
    }
    if !output.status.success() {
        let detail = first_nonempty_line(&text).unwrap_or("Gemini --delete-session failed");
        return Err(detail.to_string());
    }
    Ok(())
}

async fn run_gemini_cli_command_output(
    transport: &BackendTransport,
    workspace_root: &str,
    args: &[&str],
) -> Result<std::process::Output, String> {
    let mut cli_args: Vec<String> = args.iter().map(|arg| (*arg).to_string()).collect();
    if cli_args.is_empty() {
        cli_args.push("--help".to_string());
    }
    let shell_command = format!(
        "cd {} && PATH=\"$HOME/.cargo/bin:$HOME/.local/bin:/usr/local/bin:$PATH\" gemini {}",
        shell_quote_arg(workspace_root),
        shell_quote_command(&cli_args)
    );
    transport.run_shell_command(&shell_command).await
}

fn combine_process_output(output: &std::process::Output) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    match (stdout.is_empty(), stderr.is_empty()) {
        (true, true) => String::new(),
        (false, true) => stdout,
        (true, false) => stderr,
        (false, false) => format!("{stdout}\n{stderr}"),
    }
}

fn first_nonempty_line(text: &str) -> Option<&str> {
    text.lines().map(str::trim).find(|line| !line.is_empty())
}

fn parse_gemini_session_entries(text: &str) -> Vec<GeminiSessionListEntry> {
    let mut raw_entries: Vec<(String, String, String)> = Vec::new();

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Some((index_part, rest)) = trimmed.split_once(". ") else {
            continue;
        };
        if index_part.is_empty() || !index_part.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }

        let Some((before_id, id_part)) = rest.rsplit_once(" [") else {
            continue;
        };
        if !id_part.ends_with(']') {
            continue;
        }
        let session_id = id_part[..id_part.len() - 1].trim();
        if session_id.is_empty() {
            continue;
        }

        let preview_with_age = before_id.trim();
        let preview = if let Some((head, tail)) = preview_with_age.rsplit_once(" (") {
            if tail.ends_with(')') {
                head.trim()
            } else {
                preview_with_age
            }
        } else {
            preview_with_age
        };
        if preview.is_empty() {
            continue;
        }

        raw_entries.push((
            index_part.to_string(),
            session_id.to_string(),
            preview.to_string(),
        ));
    }

    let now = unix_now_ms();
    let total = raw_entries.len();
    raw_entries
        .into_iter()
        .enumerate()
        .map(|(pos, (index, session_id, preview))| {
            let age_slots = (total.saturating_sub(pos + 1)) as u64;
            let last_modified_ms = now.saturating_sub(age_slots.saturating_mul(1_000));
            GeminiSessionListEntry {
                index,
                session_id,
                title: preview.clone(),
                preview,
                last_modified_ms,
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// MCP settings file
// ---------------------------------------------------------------------------

fn build_gemini_settings_json(startup_mcp_servers: &[StartupMcpServer]) -> Option<String> {
    if startup_mcp_servers.is_empty() {
        return None;
    }

    let mut servers = Map::new();
    for server in startup_mcp_servers {
        let name = server.name.trim();
        if name.is_empty() {
            continue;
        }
        let config = match &server.transport {
            StartupMcpTransport::Http { url, headers, .. } => build_http_mcp_config(url, headers),
            StartupMcpTransport::Stdio { command, args, env } => {
                build_stdio_mcp_config(command, args, env)
            }
        };
        if let Some(config) = config {
            servers.insert(name.to_string(), config);
        }
    }

    if servers.is_empty() {
        return None;
    }

    Some(json!({ "mcpServers": servers }).to_string())
}

fn build_http_mcp_config(url: &str, headers: &HashMap<String, String>) -> Option<Value> {
    let trimmed_url = url.trim();
    if trimmed_url.is_empty() {
        return None;
    }
    let mut cfg = Map::new();
    cfg.insert("url".to_string(), Value::String(trimmed_url.to_string()));
    if !headers.is_empty() {
        cfg.insert(
            "headers".to_string(),
            to_value(headers).expect("HashMap<String, String> is always serializable"),
        );
    }
    Some(Value::Object(cfg))
}

fn build_stdio_mcp_config(
    command: &str,
    args: &[String],
    env: &HashMap<String, String>,
) -> Option<Value> {
    let trimmed_command = command.trim();
    if trimmed_command.is_empty() {
        return None;
    }
    let mut cfg = Map::new();
    cfg.insert(
        "command".to_string(),
        Value::String(trimmed_command.to_string()),
    );
    if !args.is_empty() {
        cfg.insert(
            "args".to_string(),
            to_value(args).expect("Vec<String> is always serializable"),
        );
    }
    if !env.is_empty() {
        cfg.insert(
            "env".to_string(),
            to_value(env).expect("HashMap<String, String> is always serializable"),
        );
    }
    Some(Value::Object(cfg))
}

fn evaluate_exit_status(
    status: ExitStatus,
    stdout_summary: GeminiStdoutSummary,
    stderr_output: &str,
) -> TurnOutcome {
    if status.code() == Some(130) {
        return TurnOutcome::Cancelled {
            summary: stdout_summary,
        };
    }
    if status.success() {
        return match stdout_summary.error_message() {
            Some(error) => TurnOutcome::Failed {
                summary: stdout_summary,
                error,
            },
            None => TurnOutcome::Completed {
                summary: stdout_summary,
            },
        };
    }
    let error = stdout_summary
        .error_message()
        .or_else(|| {
            let trimmed = stderr_output.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        })
        .unwrap_or_else(|| format!("Gemini exited with status {status}"));
    TurnOutcome::Failed {
        summary: stdout_summary,
        error,
    }
}

struct GeminiMcpCleanup {
    settings_path: std::path::PathBuf,
    original_content: Option<String>,
    created_dir: bool,
}

/// Writes MCP server config to `{workspace_root}/.gemini/settings.json`.
/// Returns a cleanup handle to restore the original state after the turn.
fn inject_gemini_mcp_settings(
    workspace_root: &str,
    json: &str,
) -> Result<GeminiMcpCleanup, String> {
    let gemini_dir = Path::new(workspace_root).join(".gemini");
    let settings_path = gemini_dir.join("settings.json");

    let created_dir = if !gemini_dir.exists() {
        fs::create_dir_all(&gemini_dir)
            .map_err(|e| format!("Failed to create .gemini directory: {e:?}"))?;
        true
    } else {
        false
    };

    let original_content = fs::read_to_string(&settings_path).ok();

    fs::write(&settings_path, json)
        .map_err(|e| format!("Failed to write .gemini/settings.json: {e:?}"))?;

    Ok(GeminiMcpCleanup {
        settings_path,
        original_content,
        created_dir,
    })
}

fn restore_gemini_mcp_settings(cleanup: Option<GeminiMcpCleanup>) {
    let Some(cleanup) = cleanup else { return };
    if let Some(original) = &cleanup.original_content {
        let _ = fs::write(&cleanup.settings_path, original);
    } else {
        let _ = fs::remove_file(&cleanup.settings_path);
        if cleanup.created_dir {
            let _ = fs::remove_dir(cleanup.settings_path.parent().unwrap());
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn pick_workspace_root(workspace_roots: &[String]) -> Result<String, String> {
    workspace_roots
        .iter()
        .find(|root| !root.trim().is_empty() && !root.starts_with("ssh://"))
        .cloned()
        .ok_or("Gemini backend requires at least one local workspace root".to_string())
}

fn normalize_optional_string(value: &Value) -> Option<String> {
    if value.is_null() {
        return None;
    }
    value
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

fn normalize_nonempty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::{timeout, Duration};

    fn event_kind(event: &Value) -> Option<&str> {
        event.get("kind").and_then(Value::as_str)
    }

    fn format_live_events(events: &[Value]) -> String {
        serde_json::to_string_pretty(&Value::Array(events.to_vec()))
            .unwrap_or_else(|_| format!("{events:?}"))
    }

    fn live_test_workspace_root() -> String {
        std::env::var("TYDE_GEMINI_TEST_WORKSPACE")
            .unwrap_or_else(|_| env!("CARGO_MANIFEST_DIR").to_string())
    }

    fn make_live_test_inner(
        workspace_root: String,
        ephemeral: bool,
    ) -> (Arc<GeminiInner>, mpsc::UnboundedReceiver<Value>) {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        (
            Arc::new(GeminiInner {
                event_tx,
                state: Mutex::new(GeminiState {
                    workspace_root,
                    transport: BackendTransport::Local,
                    ephemeral,
                    session_id: None,
                    model: Some("gemini-2.5-flash-lite".to_string()),
                    permission_mode: None,
                    steering_content: None,
                    startup_mcp_servers: Vec::new(),
                    active_turn: None,
                }),
            }),
            event_rx,
        )
    }

    async fn collect_live_gemini_events(
        prompt: &str,
        workspace_root: String,
        ephemeral: bool,
        session_id: Option<String>,
    ) -> Vec<Value> {
        let (inner, mut rx) = make_live_test_inner(workspace_root, ephemeral);
        if let Some(existing_session_id) = session_id {
            let mut state = inner.state.lock().await;
            state.session_id = Some(existing_session_id);
        }
        inner.clone().start_turn(prompt.to_string(), None).await;

        let mut events = Vec::new();
        loop {
            let event = timeout(Duration::from_secs(180), rx.recv())
                .await
                .expect("timed out waiting for live Gemini event")
                .expect("live Gemini event channel closed");
            let is_done = event_kind(&event) == Some("TypingStatusChanged")
                && event.get("data").and_then(Value::as_bool) == Some(false);
            events.push(event);
            if is_done {
                break;
            }
        }

        events
    }

    #[test]
    fn parse_gemini_usage_supports_cached_and_non_cached_input_fields() {
        let raw = json!({
            "total_tokens": 11050,
            "input_tokens": 10856,
            "input": 8970,
            "output_tokens": 38,
            "cached": 1886,
            "duration_ms": 8500,
            "tool_calls": 0
        });

        let usage = parse_gemini_usage(Some(&raw)).expect("usage should parse");
        assert_eq!(
            usage.get("input_tokens").and_then(Value::as_u64),
            Some(8970)
        );
        assert_eq!(
            usage.get("cached_prompt_tokens").and_then(Value::as_u64),
            Some(1886)
        );
        assert_eq!(usage.get("output_tokens").and_then(Value::as_u64), Some(38));
    }

    #[test]
    fn parse_gemini_session_entries_parses_cli_list_output() {
        let output = r#"
Keychain initialization encountered an error: An unknown error occurred.
Using FileKeychain fallback for secure storage.

Available sessions for this project (2):
  1. old prompt (3 days ago) [11111111-1111-1111-1111-111111111111]
  2. latest prompt (Just now) [22222222-2222-2222-2222-222222222222]
"#;

        let sessions = parse_gemini_session_entries(output);
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].index, "1");
        assert_eq!(
            sessions[0].session_id,
            "11111111-1111-1111-1111-111111111111"
        );
        assert_eq!(sessions[0].preview, "old prompt");
        assert_eq!(sessions[1].index, "2");
        assert_eq!(
            sessions[1].session_id,
            "22222222-2222-2222-2222-222222222222"
        );
        assert_eq!(sessions[1].preview, "latest prompt");
        assert!(sessions[1].last_modified_ms >= sessions[0].last_modified_ms);
    }

    #[test]
    fn estimate_gemini_context_breakdown_returns_context_window() {
        let usage = json!({
            "input_tokens": 9000,
            "cached_prompt_tokens": 1000,
            "cache_creation_input_tokens": 0,
            "reasoning_tokens": 120
        });
        let breakdown =
            estimate_gemini_context_breakdown(Some(&usage), Some("gemini-2.5-flash-lite"))
                .expect("breakdown should be present");
        assert_eq!(
            breakdown.get("input_tokens").and_then(Value::as_u64),
            Some(10_000)
        );
        assert!(
            breakdown
                .get("context_window")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                >= 10_000
        );
    }

    #[tokio::test]
    #[ignore = "requires local Gemini CLI auth and network; set TYDE_RUN_GEMINI_INTEGRATION=1"]
    async fn live_gemini_stream_end_includes_usage_and_context_breakdown() {
        if std::env::var("TYDE_RUN_GEMINI_INTEGRATION").ok().as_deref() != Some("1") {
            eprintln!("Skipping live Gemini integration test; set TYDE_RUN_GEMINI_INTEGRATION=1");
            return;
        }

        let marker = format!("LIVE_GEMINI_PARSING_{}", unix_now_ms());
        let prompt = format!("Respond with exactly: {marker}");
        let events =
            collect_live_gemini_events(&prompt, live_test_workspace_root(), true, None).await;
        let events_dump = format_live_events(&events);

        let stream_end = events
            .iter()
            .find(|event| event_kind(event) == Some("StreamEnd"))
            .unwrap_or_else(|| panic!("Expected StreamEnd event. Events:\n{events_dump}"));

        let message = stream_end
            .get("data")
            .and_then(|v| v.get("message"))
            .unwrap_or_else(|| panic!("StreamEnd missing data.message. Events:\n{events_dump}"));
        let content = message
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string();
        assert!(
            content.contains(&marker),
            "Expected StreamEnd content to contain marker {marker}. content={content:?}\nEvents:\n{events_dump}"
        );

        let usage = message
            .get("token_usage")
            .and_then(Value::as_object)
            .unwrap_or_else(|| {
                panic!("Expected token_usage object in StreamEnd. Events:\n{events_dump}")
            });

        assert!(
            usage
                .get("input_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                > 0,
            "Expected input_tokens > 0 in token_usage. token_usage={usage:?}"
        );
        assert!(
            usage
                .get("output_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                > 0,
            "Expected output_tokens > 0 in token_usage. token_usage={usage:?}"
        );
        assert!(
            usage
                .get("cached_prompt_tokens")
                .and_then(Value::as_u64)
                .is_some(),
            "Expected cached_prompt_tokens field in token_usage. token_usage={usage:?}"
        );

        let context_breakdown = message
            .get("context_breakdown")
            .and_then(Value::as_object)
            .unwrap_or_else(|| {
                panic!("Expected context_breakdown object in StreamEnd. Events:\n{events_dump}")
            });
        assert!(
            context_breakdown
                .get("context_window")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                > 0,
            "Expected context_window > 0 in context_breakdown. context_breakdown={context_breakdown:?}"
        );
        assert!(
            context_breakdown
                .get("input_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                > 0,
            "Expected input_tokens > 0 in context_breakdown. context_breakdown={context_breakdown:?}"
        );
    }

    #[tokio::test]
    #[ignore = "requires local Gemini CLI auth and network; set TYDE_RUN_GEMINI_INTEGRATION=1"]
    async fn live_gemini_resume_reuses_session_id_and_keeps_usage_fields() {
        if std::env::var("TYDE_RUN_GEMINI_INTEGRATION").ok().as_deref() != Some("1") {
            eprintln!("Skipping live Gemini integration test; set TYDE_RUN_GEMINI_INTEGRATION=1");
            return;
        }

        let workspace_root = live_test_workspace_root();
        let marker = format!("LIVE_GEMINI_RESUME_{}", unix_now_ms());
        let first_prompt = format!("Respond with exactly: {marker}");
        let first_events =
            collect_live_gemini_events(&first_prompt, workspace_root.clone(), false, None).await;
        let first_events_dump = format_live_events(&first_events);
        let session_id = first_events
            .iter()
            .find_map(|event| {
                if event_kind(event) != Some("SessionStarted") {
                    return None;
                }
                event.get("data")
                    .and_then(|v| v.get("session_id"))
                    .and_then(Value::as_str)
                    .map(|s| s.to_string())
            })
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| {
                panic!("Expected SessionStarted with non-empty session_id. Events:\n{first_events_dump}")
            });

        let second_prompt =
            "What exact token did I ask you to output in the previous message? Reply with one token only.";
        let second_events = collect_live_gemini_events(
            second_prompt,
            workspace_root,
            false,
            Some(session_id.clone()),
        )
        .await;
        let second_events_dump = format_live_events(&second_events);

        let resumed_session_id = second_events
            .iter()
            .find_map(|event| {
                if event_kind(event) != Some("SessionStarted") {
                    return None;
                }
                event
                    .get("data")
                    .and_then(|v| v.get("session_id"))
                    .and_then(Value::as_str)
                    .map(|s| s.to_string())
            })
            .unwrap_or_else(|| {
                panic!("Expected SessionStarted in resumed turn. Events:\n{second_events_dump}")
            });
        assert_eq!(
            resumed_session_id, session_id,
            "Expected resumed turn to keep Gemini session id"
        );

        let stream_end = second_events
            .iter()
            .find(|event| event_kind(event) == Some("StreamEnd"))
            .unwrap_or_else(|| {
                panic!("Expected StreamEnd in resumed turn. Events:\n{second_events_dump}")
            });
        let message = stream_end
            .get("data")
            .and_then(|v| v.get("message"))
            .unwrap_or_else(|| {
                panic!("StreamEnd missing data.message. Events:\n{second_events_dump}")
            });
        let content = message
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string();
        assert!(
            content.contains(&marker),
            "Expected resumed answer to include marker {marker}. content={content:?}\nEvents:\n{second_events_dump}"
        );

        let usage = message
            .get("token_usage")
            .and_then(Value::as_object)
            .unwrap_or_else(|| panic!("Expected token_usage object in resumed StreamEnd. Events:\n{second_events_dump}"));
        assert!(
            usage
                .get("input_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                > 0,
            "Expected input_tokens > 0 in resumed token_usage. token_usage={usage:?}"
        );
        assert!(
            usage
                .get("cached_prompt_tokens")
                .and_then(Value::as_u64)
                .is_some(),
            "Expected cached_prompt_tokens field in resumed token_usage. token_usage={usage:?}"
        );
    }
}
