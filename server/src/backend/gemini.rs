use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, from_str, json, to_value};
use tokio::io::AsyncBufReadExt;
use tokio::io::BufReader;
use tokio::process::{ChildStderr, ChildStdout, Command};
use tokio::sync::{Mutex, mpsc, oneshot};

use crate::backend::{
    SessionCommand, StartupMcpServer, StartupMcpTransport, render_combined_spawn_instructions,
};
use crate::process_env;
use crate::remote::{
    parse_remote_workspace_roots, shell_quote_arg, shell_quote_command, ssh_control_args,
};
use crate::subprocess::ImageAttachment;

const GEMINI_AGENT_NAME: &str = "gemini";
const GEMINI_DEFAULT_PERMISSION_MODE: &str = "bypassPermissions";
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
        ssh_host: Option<String>,
        startup_mcp_servers: &[StartupMcpServer],
        steering_content: Option<&str>,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Value>), String> {
        Self::spawn_with_mode(
            workspace_roots,
            ssh_host,
            startup_mcp_servers,
            steering_content,
        )
        .await
    }

    pub async fn spawn_ephemeral(
        workspace_roots: &[String],
        ssh_host: Option<String>,
        startup_mcp_servers: &[StartupMcpServer],
        steering_content: Option<&str>,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Value>), String> {
        Self::spawn_with_mode(
            workspace_roots,
            ssh_host,
            startup_mcp_servers,
            steering_content,
        )
        .await
    }

    pub async fn spawn_admin(
        workspace_roots: &[String],
        ssh_host: Option<String>,
        startup_mcp_servers: &[StartupMcpServer],
        steering_content: Option<&str>,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Value>), String> {
        Self::spawn_with_mode(
            workspace_roots,
            ssh_host,
            startup_mcp_servers,
            steering_content,
        )
        .await
    }

    async fn spawn_with_mode(
        workspace_roots: &[String],
        ssh_host: Option<String>,
        startup_mcp_servers: &[StartupMcpServer],
        steering_content: Option<&str>,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Value>), String> {
        let (workspace_root, resolved_ssh_host) = if let Some(host) = ssh_host {
            let parsed = parse_remote_workspace_roots(workspace_roots)?
                .ok_or("Expected remote workspace roots for SSH session")?;
            let remote_path = parsed
                .1
                .into_iter()
                .next()
                .ok_or("No remote workspace root found")?;
            (remote_path, Some(host))
        } else {
            (pick_workspace_root(workspace_roots)?, None)
        };

        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let inner = Arc::new(GeminiInner {
            event_tx,
            state: Mutex::new(GeminiState {
                workspace_root,
                ssh_host: resolved_ssh_host,
                session_id: None,
                model: None,
                permission_mode: Some(GEMINI_DEFAULT_PERMISSION_MODE.to_string()),
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
    ssh_host: Option<String>,
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
                    "data": {
                        "models": gemini_known_models()
                            .into_iter()
                            .map(|model| json!({
                                "id": model.value,
                                "displayName": model.label,
                                "isDefault": false,
                            }))
                            .collect::<Vec<_>>()
                    }
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
            SessionCommand::ListSessions => {
                this.emit_event(json!({
                    "kind": "SessionsList",
                    "data": { "sessions": [] }
                }));
                Ok(())
            }
            SessionCommand::ResumeSession { session_id } => {
                let normalized = normalize_optional_string(&Value::String(session_id))
                    .ok_or("Invalid session id")?;
                {
                    let mut state = this.state.lock().await;
                    state.session_id = Some(normalized.clone());
                }
                this.emit_event(json!({
                    "kind": "SessionStarted",
                    "data": { "session_id": normalized }
                }));
                Ok(())
            }
            SessionCommand::DeleteSession { session_id } => {
                let normalized = normalize_optional_string(&Value::String(session_id))
                    .ok_or("Invalid session id")?;
                let mut state = this.state.lock().await;
                if state.session_id.as_deref() == Some(normalized.as_str()) {
                    state.session_id = None;
                }
                Ok(())
            }
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
            ssh_host,
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
                state.ssh_host.clone(),
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
                    ssh_host.as_deref(),
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
                        self.set_session_id(turn_id, session_id.clone()).await;
                        self.emit_event(json!({
                            "kind": "SessionStarted",
                            "data": { "session_id": session_id }
                        }));
                    }
                    if !self.emit_summary_or_placeholder_stream_end(&mut summary) {
                        let error = summary
                            .error_message()
                            .unwrap_or_else(|| "Gemini returned no assistant output.".to_string());
                        self.emit_error(&error);
                    }
                }
                TurnOutcome::Cancelled { mut summary } => {
                    if let Some(session_id) = summary.session_id.clone() {
                        self.set_session_id(turn_id, session_id).await;
                    }
                    self.emit_summary_or_placeholder_stream_end(&mut summary);
                    self.emit_operation_cancelled("Gemini turn cancelled.");
                }
                TurnOutcome::Failed { summary, error } => {
                    let mut summary = summary;
                    self.emit_summary_or_placeholder_stream_end(&mut summary);
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
        ssh_host: Option<&str>,
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

        if let Some(model_name) = model.as_deref().filter(|m| !m.trim().is_empty()) {
            cli_args.push("--model".to_string());
            cli_args.push(model_name.to_string());
        }
        if let Some(session_id) = session_id.as_deref().filter(|id| !id.trim().is_empty()) {
            cli_args.push("--resume".to_string());
            cli_args.push(session_id.to_string());
        }

        // Gemini CLI reads MCP config from {workspace_root}/.gemini/settings.json.
        // Inject startup MCP servers by writing that file, restoring original after.
        let mcp_settings_json = build_gemini_settings_json(startup_mcp_servers);
        let mut mcp_cleanup: Option<GeminiMcpCleanup> = None;

        if let Some(ref json) = mcp_settings_json
            && ssh_host.is_none()
        {
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

        let mut command = if let Some(host) = ssh_host {
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
            let mut cmd = Command::new("ssh");
            let control_args = match ssh_control_args() {
                Ok(args) => args,
                Err(err) => {
                    restore_gemini_mcp_settings(mcp_cleanup.take());
                    return TurnOutcome::Failed {
                        summary: GeminiStdoutSummary::default(),
                        error: format!("Failed to get SSH control args: {err}"),
                    };
                }
            };
            for arg in control_args {
                cmd.arg(arg);
            }
            cmd.arg("-T")
                .arg(host)
                .arg(remote_cmd)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            cmd
        } else {
            let mut cmd = Command::new("gemini");
            for arg in &cli_args {
                cmd.arg(arg);
            }
            if let Some(path) = process_env::resolved_child_process_path() {
                cmd.env("PATH", path);
            }
            cmd.current_dir(workspace_root)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            cmd
        };

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(err) => {
                restore_gemini_mcp_settings(mcp_cleanup.take());
                return TurnOutcome::Failed {
                    summary: GeminiStdoutSummary::default(),
                    error: format!("Failed to start Gemini CLI: {err:?}"),
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

    async fn set_session_id(&self, turn_id: u64, session_id: String) {
        let mut state = self.state.lock().await;
        if state
            .active_turn
            .as_ref()
            .is_some_and(|active| active.id == turn_id)
        {
            state.session_id = Some(session_id);
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

    async fn emit_settings(&self) {
        let (model, permission_mode) = {
            let state = self.state.lock().await;
            (state.model.clone(), state.permission_mode.clone())
        };
        self.emit_event(json!({
            "kind": "Settings",
            "data": {
                "model": model,
                "permission_mode": permission_mode,
            }
        }));
    }

    // -----------------------------------------------------------------------
    // Event emission helpers
    // -----------------------------------------------------------------------

    fn emit_summary_and_tool_requests(&self, summary: &mut GeminiStdoutSummary) -> bool {
        let text = summary.streamed_text.trim().to_string();
        let reasoning = preserve_nonempty_whitespace(&summary.streamed_reasoning);
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

    fn emit_summary_or_placeholder_stream_end(&self, summary: &mut GeminiStdoutSummary) -> bool {
        if self.emit_summary_and_tool_requests(summary) {
            return true;
        }

        self.emit_stream_end(
            String::new(),
            summary.model.clone(),
            summary.usage.take(),
            None,
            Vec::new(),
        );
        false
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
        let model_info = model
            .filter(|m| !m.trim().is_empty())
            .map(|m| json!({ "model": m }))
            .unwrap_or(Value::Null);
        let usage_value = usage.unwrap_or(Value::Null);
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
                    "context_breakdown": Value::Null,
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
    if let Some(session_id) = value
        .get("session_id")
        .or_else(|| value.get("sessionId"))
        .and_then(Value::as_str)
    {
        let is_new_session = summary.session_id.as_deref() != Some(session_id);
        summary.session_id = Some(session_id.to_string());
        if is_new_session {
            inner.emit_event(json!({
                "kind": "SessionStarted",
                "data": { "session_id": session_id }
            }));
        }
    }

    let event_type = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();

    match event_type {
        "init" => {
            if let Some(model) = value.get("model").and_then(Value::as_str) {
                summary.model = Some(model.to_string());
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
            if let Some(session_id) = value
                .get("session_id")
                .or_else(|| value.get("sessionId"))
                .and_then(Value::as_str)
            {
                summary.session_id = Some(session_id.to_string());
            }
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
    extract_text_fragment(value.get("content"))
        .or_else(|| extract_text_fragment(value.get("text")))
        .or_else(|| value.get("message").and_then(extract_message_text))
}

fn extract_text_fragment(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(text) if !text.is_empty() => Some(text.clone()),
        Value::Array(items) => {
            let combined = items
                .iter()
                .filter_map(|item| extract_text_fragment(Some(item)))
                .collect::<String>();
            (!combined.is_empty()).then_some(combined)
        }
        Value::Object(map) => extract_text_fragment(map.get("text"))
            .or_else(|| extract_text_fragment(map.get("content")))
            .or_else(|| extract_text_fragment(map.get("parts")))
            .or_else(|| extract_text_fragment(map.get("delta")))
            .or_else(|| extract_text_fragment(map.get("message"))),
        _ => None,
    }
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
    let reasoning = preserve_nonempty_whitespace(&summary.streamed_reasoning);
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

fn preserve_nonempty_whitespace(text: &str) -> Option<String> {
    text.chars()
        .any(|ch| !ch.is_whitespace())
        .then(|| text.to_string())
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

fn gemini_known_models() -> Vec<SelectOption> {
    vec![
        SelectOption {
            value: "gemini-2.5-pro".to_string(),
            label: "Gemini 2.5 Pro".to_string(),
        },
        SelectOption {
            value: "gemini-2.5-flash".to_string(),
            label: "Gemini 2.5 Flash".to_string(),
        },
        SelectOption {
            value: "gemini-2.5-flash-lite".to_string(),
            label: "Gemini 2.5 Flash Lite".to_string(),
        },
    ]
}

// ---------------------------------------------------------------------------
// Usage parsing
// ---------------------------------------------------------------------------

fn parse_gemini_usage(raw: Option<&Value>) -> Option<Value> {
    let stats = raw?.as_object()?;

    let input_tokens = stats
        .get("input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output_tokens = stats
        .get("output_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let total_tokens = stats
        .get("total_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(input_tokens.saturating_add(output_tokens));
    let cached_tokens = stats
        .get("cached_tokens")
        .or_else(|| stats.get("cached_prompt_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let reasoning_tokens = stats
        .get("reasoning_tokens")
        .or_else(|| stats.get("thoughts_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let duration_ms = stats
        .get("duration_ms")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let tool_call_count = stats.get("tool_calls").and_then(Value::as_u64).unwrap_or(0);

    if input_tokens == 0 && output_tokens == 0 && total_tokens == 0 {
        return None;
    }

    Some(json!({
        "input_tokens": input_tokens,
        "output_tokens": output_tokens,
        "total_tokens": total_tokens,
        "cached_prompt_tokens": cached_tokens,
        "cache_creation_input_tokens": 0,
        "reasoning_tokens": reasoning_tokens,
        "duration_ms": duration_ms,
        "tool_calls": tool_call_count,
    }))
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

// ===========================================================================
// Backend trait implementation
// ===========================================================================

use protocol::{
    AgentInput, BackendKind, ChatEvent, ChatMessage, MessageSender, SelectOption, SessionId,
    SessionSettingField, SessionSettingFieldType, SessionSettingValue, SessionSettingsSchema,
    SpawnCostHint,
};

use super::{
    Backend, BackendSession, BackendSpawnConfig, EventStream, protocol_images_to_attachments,
    resolve_settings as resolve_backend_settings, session_settings_to_json,
};

const EVENT_BUFFER: usize = 256;
const GEMINI_SPAWN_TIMEOUT: Duration = Duration::from_secs(120);

type GeminiReadyTx = Arc<Mutex<Option<oneshot::Sender<Result<SessionId, String>>>>>;

pub struct GeminiBackend {
    input_tx: mpsc::Sender<AgentInput>,
    interrupt_tx: mpsc::Sender<()>,
    session_id: Arc<std::sync::Mutex<Option<SessionId>>>,
}

fn gemini_backend_model(cost_hint: Option<SpawnCostHint>) -> Option<&'static str> {
    match cost_hint {
        Some(SpawnCostHint::Low) => Some("gemini-2.5-flash-lite"),
        Some(SpawnCostHint::Medium) => Some("gemini-2.5-flash"),
        Some(SpawnCostHint::High) => Some("gemini-2.5-pro"),
        None => None,
    }
}

fn gemini_cost_hint_defaults(cost_hint: SpawnCostHint) -> protocol::SessionSettingsValues {
    let mut values = protocol::SessionSettingsValues::default();
    if let Some(model) = gemini_backend_model(Some(cost_hint)) {
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
        &GeminiBackend::session_settings_schema(),
        gemini_cost_hint_defaults,
    )
}

fn backend_error_message(content: String) -> ChatEvent {
    ChatEvent::MessageAdded(ChatMessage {
        timestamp: unix_now_ms(),
        sender: MessageSender::Error,
        content,
        reasoning: None,
        tool_calls: Vec::new(),
        model_info: None,
        token_usage: None,
        context_breakdown: None,
        images: None,
    })
}

async fn forward_gemini_backend_event(
    raw: Value,
    events_tx: &mpsc::Sender<ChatEvent>,
    session_id_sink: &Arc<std::sync::Mutex<Option<SessionId>>>,
    ready_tx: Option<&GeminiReadyTx>,
) -> bool {
    if let Ok(event) = serde_json::from_value::<ChatEvent>(raw.clone()) {
        return events_tx.send(event).await.is_ok();
    }

    match raw.get("kind").and_then(Value::as_str).unwrap_or_default() {
        "SessionStarted" => {
            if let Some(session_id) = raw
                .get("data")
                .and_then(|data| data.get("session_id"))
                .and_then(Value::as_str)
            {
                let session = SessionId(session_id.to_string());
                *session_id_sink
                    .lock()
                    .expect("gemini session_id mutex poisoned") = Some(session.clone());
                if let Some(ready_tx) = ready_tx {
                    let mut ready_tx = ready_tx.lock().await;
                    if let Some(tx) = ready_tx.take() {
                        let _ = tx.send(Ok(session));
                    }
                }
            }
        }
        "Error" => {
            let message = raw
                .get("data")
                .and_then(Value::as_str)
                .unwrap_or("Gemini backend error")
                .to_string();
            let session_started = session_id_sink
                .lock()
                .expect("gemini session_id mutex poisoned")
                .is_some();
            if !session_started && let Some(ready_tx) = ready_tx {
                let mut ready_tx = ready_tx.lock().await;
                if let Some(tx) = ready_tx.take() {
                    let _ = tx.send(Err(message.clone()));
                }
            }
            if events_tx
                .send(backend_error_message(message.clone()))
                .await
                .is_err()
            {
                return false;
            }
        }
        _ => {}
    }

    true
}

impl Backend for GeminiBackend {
    fn session_settings_schema() -> SessionSettingsSchema {
        SessionSettingsSchema {
            backend_kind: BackendKind::Gemini,
            fields: vec![SessionSettingField {
                key: "model".to_string(),
                label: "Model".to_string(),
                description: None,
                use_slider: false,
                field_type: SessionSettingFieldType::Select {
                    options: gemini_known_models(),
                    default: None,
                    nullable: true,
                },
            }],
        }
    }

    async fn spawn(
        workspace_roots: Vec<String>,
        config: BackendSpawnConfig,
        initial_input: protocol::SendMessagePayload,
    ) -> Result<(Self, EventStream), String> {
        let (input_tx, mut input_rx) = mpsc::channel::<AgentInput>(EVENT_BUFFER);
        let (interrupt_tx, mut interrupt_rx) = mpsc::channel::<()>(EVENT_BUFFER);
        let (events_tx, events_rx) = mpsc::channel::<ChatEvent>(EVENT_BUFFER);
        let session_id = Arc::new(std::sync::Mutex::new(None));
        let session_id_task = Arc::clone(&session_id);
        let (ready_tx, ready_rx) = oneshot::channel::<Result<SessionId, String>>();

        tokio::spawn(async move {
            let roots = if workspace_roots.is_empty() {
                vec!["/tmp".to_string()]
            } else {
                workspace_roots
            };
            let combined_instructions =
                render_combined_spawn_instructions(&config.resolved_spawn_config);
            let (session, mut raw_events) = match GeminiSession::spawn(
                &roots,
                None,
                &config.startup_mcp_servers,
                combined_instructions.as_deref(),
            )
            .await
            {
                Ok(value) => value,
                Err(err) => {
                    tracing::error!("Failed to spawn Gemini session: {err}");
                    let _ = ready_tx.send(Err(format!("Failed to spawn Gemini session: {err}")));
                    return;
                }
            };

            let handle = session.command_handle();
            let resolved_settings = resolve_session_settings(&config);
            let model_override = match resolved_settings.0.get("model") {
                Some(SessionSettingValue::String(value)) => Some(value.clone()),
                _ => None,
            };
            if model_override.is_some() {
                let settings = json!({
                    "model": model_override,
                    "permission_mode": GEMINI_DEFAULT_PERMISSION_MODE,
                });
                if let Err(err) = handle
                    .execute(SessionCommand::UpdateSettings {
                        settings,
                        persist: false,
                    })
                    .await
                {
                    tracing::error!("Failed to configure Gemini session: {err}");
                    let _ =
                        ready_tx.send(Err(format!("Failed to configure Gemini session: {err}")));
                    session.shutdown().await;
                    return;
                }
            }

            let ready_tx: GeminiReadyTx = Arc::new(Mutex::new(Some(ready_tx)));
            let ready_tx_forward = Arc::clone(&ready_tx);
            let session_id_forward = Arc::clone(&session_id_task);
            let events_tx_forward = events_tx.clone();
            let forward_task = tokio::spawn(async move {
                while let Some(raw) = raw_events.recv().await {
                    if !forward_gemini_backend_event(
                        raw,
                        &events_tx_forward,
                        &session_id_forward,
                        Some(&ready_tx_forward),
                    )
                    .await
                    {
                        return;
                    }
                }
                let mut ready_tx = ready_tx_forward.lock().await;
                if let Some(tx) = ready_tx.take() {
                    let _ = tx.send(Err(
                        "Gemini session ended before reporting a session_id".to_string()
                    ));
                }
            });

            if let Err(err) = handle
                .execute(SessionCommand::SendMessage {
                    message: initial_input.message,
                    images: protocol_images_to_attachments(initial_input.images),
                })
                .await
            {
                tracing::error!("Failed to send initial Gemini prompt: {err}");
                let mut ready_tx = ready_tx.lock().await;
                if let Some(tx) = ready_tx.take() {
                    let _ = tx.send(Err(format!("Failed to send initial Gemini prompt: {err}")));
                }
                session.shutdown().await;
                let _ = forward_task.await;
                return;
            }

            loop {
                tokio::select! {
                    incoming = input_rx.recv() => {
                        let Some(input) = incoming else {
                            break;
                        };
                        match input {
                            AgentInput::SendMessage(payload) => {
                                let images = protocol_images_to_attachments(payload.images);
                                if let Err(err) = handle
                                    .execute(SessionCommand::SendMessage {
                                        message: payload.message,
                                        images,
                                    })
                                    .await
                                {
                                    tracing::error!("Failed to send Gemini follow-up: {err}");
                                    break;
                                }
                            }
                            AgentInput::UpdateSessionSettings(payload) => {
                                if let Err(err) = handle
                                    .execute(SessionCommand::UpdateSettings {
                                        settings: session_settings_to_json(&payload.values),
                                        persist: false,
                                    })
                                    .await
                                {
                                    tracing::error!("Failed to update Gemini session settings: {err}");
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
                        let Some(()) = interrupt else {
                            break;
                        };
                        if let Err(err) = handle.execute(SessionCommand::CancelConversation).await {
                            tracing::error!("Failed to interrupt Gemini turn: {err}");
                            break;
                        }
                        if events_tx
                            .send(ChatEvent::OperationCancelled(
                                protocol::OperationCancelledData {
                                    message: "Gemini turn cancelled.".to_string(),
                                },
                            ))
                            .await
                            .is_err()
                        {
                            break;
                        }
                        if events_tx
                            .send(ChatEvent::TypingStatusChanged(false))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }

            session.shutdown().await;
            let _ = forward_task.await;
        });

        match tokio::time::timeout(GEMINI_SPAWN_TIMEOUT, ready_rx).await {
            Ok(Ok(Ok(session))) => {
                *session_id.lock().expect("gemini session_id mutex poisoned") = Some(session);
            }
            Ok(Ok(Err(err))) => return Err(err),
            Ok(Err(_)) => {
                return Err("Gemini spawn initialization task ended early".to_string());
            }
            Err(_) => return Err("Timed out waiting for Gemini session_id".to_string()),
        }

        Ok((
            Self {
                input_tx,
                interrupt_tx,
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
        let (input_tx, mut input_rx) = mpsc::channel::<AgentInput>(EVENT_BUFFER);
        let (interrupt_tx, mut interrupt_rx) = mpsc::channel::<()>(EVENT_BUFFER);
        let (events_tx, events_rx) = mpsc::channel::<ChatEvent>(EVENT_BUFFER);
        let roots = if workspace_roots.is_empty() {
            vec!["/tmp".to_string()]
        } else {
            workspace_roots
        };
        let backend_session_id = Arc::new(std::sync::Mutex::new(Some(session_id.clone())));
        let backend_session_id_task = Arc::clone(&backend_session_id);

        tokio::spawn(async move {
            let combined_instructions =
                render_combined_spawn_instructions(&config.resolved_spawn_config);
            let (session, mut raw_events) = match GeminiSession::spawn(
                &roots,
                None,
                &config.startup_mcp_servers,
                combined_instructions.as_deref(),
            )
            .await
            {
                Ok(value) => value,
                Err(err) => {
                    tracing::error!("Failed to spawn Gemini resume session: {err}");
                    return;
                }
            };

            let handle = session.command_handle();
            let resolved_settings = resolve_session_settings(&config);
            let model_override = match resolved_settings.0.get("model") {
                Some(SessionSettingValue::String(value)) => Some(value.clone()),
                _ => None,
            };
            if model_override.is_some() {
                let settings = json!({
                    "model": model_override,
                    "permission_mode": GEMINI_DEFAULT_PERMISSION_MODE,
                });
                if let Err(err) = handle
                    .execute(SessionCommand::UpdateSettings {
                        settings,
                        persist: false,
                    })
                    .await
                {
                    tracing::error!("Failed to configure resumed Gemini session: {err}");
                    session.shutdown().await;
                    return;
                }
            }

            if let Err(err) = handle
                .execute(SessionCommand::ResumeSession {
                    session_id: session_id.0,
                })
                .await
            {
                tracing::error!("Failed to resume Gemini session: {err}");
                session.shutdown().await;
                return;
            }

            loop {
                tokio::select! {
                    incoming = raw_events.recv() => {
                        let Some(raw) = incoming else {
                            break;
                        };
                        if !forward_gemini_backend_event(raw, &events_tx, &backend_session_id_task, None).await {
                            break;
                        }
                    }
                    input = input_rx.recv() => {
                        let Some(input) = input else {
                            break;
                        };
                        match input {
                            AgentInput::SendMessage(payload) => {
                                let images = protocol_images_to_attachments(payload.images);
                                if let Err(err) = handle
                                    .execute(SessionCommand::SendMessage {
                                        message: payload.message,
                                        images,
                                    })
                                    .await
                                {
                                    tracing::error!("Failed to send Gemini resume follow-up: {err}");
                                    break;
                                }
                            }
                            AgentInput::UpdateSessionSettings(payload) => {
                                if let Err(err) = handle
                                    .execute(SessionCommand::UpdateSettings {
                                        settings: session_settings_to_json(&payload.values),
                                        persist: false,
                                    })
                                    .await
                                {
                                    tracing::error!("Failed to update resumed Gemini session settings: {err}");
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
                        let Some(()) = interrupt else {
                            break;
                        };
                        if let Err(err) = handle.execute(SessionCommand::CancelConversation).await {
                            tracing::error!("Failed to interrupt resumed Gemini turn: {err}");
                            break;
                        }
                        if events_tx
                            .send(ChatEvent::OperationCancelled(
                                protocol::OperationCancelledData {
                                    message: "Gemini turn cancelled.".to_string(),
                                },
                            ))
                            .await
                            .is_err()
                        {
                            break;
                        }
                        if events_tx
                            .send(ChatEvent::TypingStatusChanged(false))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }

            session.shutdown().await;
        });

        Ok((
            Self {
                input_tx,
                interrupt_tx,
                session_id: backend_session_id,
            },
            EventStream::new(events_rx),
        ))
    }

    async fn list_sessions() -> Result<Vec<BackendSession>, String> {
        Err("GeminiBackend::list_sessions is not supported without workspace context".to_string())
    }

    async fn send(&self, input: AgentInput) -> bool {
        self.input_tx.send(input).await.is_ok()
    }

    fn session_id(&self) -> SessionId {
        self.session_id
            .lock()
            .expect("gemini session_id mutex poisoned")
            .clone()
            .expect("gemini session_id not initialized")
    }

    async fn interrupt(&self) -> bool {
        self.interrupt_tx.send(()).await.is_ok()
    }

    async fn shutdown(self) {
        drop(self);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn spawn_defaults_permission_mode_to_bypass_permissions() {
        let workspace = std::env::temp_dir().join(format!("tyde-gemini-test-{}", unix_now_ms()));
        std::fs::create_dir_all(&workspace).expect("create temp workspace");
        let roots = vec![workspace.display().to_string()];
        let (session, _events) = GeminiSession::spawn(&roots, None, &[], None)
            .await
            .expect("spawn Gemini session");

        let state = session.inner.state.lock().await;
        assert_eq!(
            state.permission_mode.as_deref(),
            Some(GEMINI_DEFAULT_PERMISSION_MODE)
        );
    }
}
