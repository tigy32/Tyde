use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::fs as tokio_fs;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdout, Command};
use tokio::sync::{Mutex, mpsc, oneshot, watch};

use protocol::ToolPolicy;

use crate::backend::{AgentIdentity, SessionCommand, StartupMcpServer, StartupMcpTransport};
use crate::process_env;
use crate::sub_agent::SubAgentEmitter;
#[cfg(test)]
use crate::sub_agent::SubAgentHandle;
use crate::subprocess::ImageAttachment;

/// Per-sub-agent stream state, tracking its own summary and segment.
struct SubAgentStream {
    summary: ClaudeStdoutSummary,
    segment: SegmentState,
    message_id: String,
    has_explicit_task_prompt: bool,
    /// A local ClaudeInner that routes events to the sub-agent's channel.
    inner: Arc<ClaudeInner>,
}

#[derive(Default)]
struct PendingSubAgentPrompt {
    tool_use_id: String,
    partial_json: String,
}

const CLAUDE_AGENT_NAME: &str = "claude";
const CLAUDE_ESTIMATED_CONTEXT_WINDOW_DEFAULT: u64 = 200_000;
const CLAUDE_ESTIMATED_CONTEXT_WINDOW_1M: u64 = 1_000_000;
const CLAUDE_ESTIMATED_BYTES_PER_TOKEN: u64 = 4;
const CLAUDE_MIN_SYSTEM_PROMPT_BYTES: u64 = 1_024;
const CLAUDE_DEFAULT_PERMISSION_MODE: &str = "bypassPermissions";
const CLAUDE_CONVERSATION_COMPACTED_NOTICE: &str = "Conversation compacted.";
static CLAUDE_TURN_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Clone)]
pub struct ClaudeCommandHandle {
    inner: Arc<ClaudeInner>,
}

impl ClaudeCommandHandle {
    pub async fn execute(&self, command: SessionCommand) -> Result<(), String> {
        ClaudeInner::execute_arc(Arc::clone(&self.inner), command).await
    }
}

#[derive(Clone)]
pub struct ClaudeSession {
    inner: Arc<ClaudeInner>,
}

impl ClaudeSession {
    pub async fn spawn(
        workspace_roots: &[String],
        ssh_host: Option<String>,
        startup_mcp_servers: &[StartupMcpServer],
        steering_content: Option<&str>,
        agent_identity: Option<&AgentIdentity>,
        tool_policy: ToolPolicy,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Value>), String> {
        Self::spawn_with_mode(
            workspace_roots,
            false,
            ssh_host,
            startup_mcp_servers,
            steering_content,
            agent_identity,
            tool_policy,
        )
        .await
    }

    pub async fn spawn_ephemeral(
        workspace_roots: &[String],
        ssh_host: Option<String>,
        startup_mcp_servers: &[StartupMcpServer],
        steering_content: Option<&str>,
        agent_identity: Option<&AgentIdentity>,
        tool_policy: ToolPolicy,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Value>), String> {
        Self::spawn_with_mode(
            workspace_roots,
            true,
            ssh_host,
            startup_mcp_servers,
            steering_content,
            agent_identity,
            tool_policy,
        )
        .await
    }

    async fn spawn_with_mode(
        workspace_roots: &[String],
        no_session_persistence: bool,
        ssh_host: Option<String>,
        startup_mcp_servers: &[StartupMcpServer],
        steering_content: Option<&str>,
        agent_identity: Option<&AgentIdentity>,
        tool_policy: ToolPolicy,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Value>), String> {
        let (workspace_root, resolved_ssh_host) = if let Some(host) = ssh_host {
            let parsed = crate::remote::parse_remote_workspace_roots(workspace_roots)?
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

        let inner = Arc::new(ClaudeInner {
            event_tx,
            state: Mutex::new(ClaudeState {
                workspace_root,
                ssh_host: resolved_ssh_host,
                session_id: None,
                ephemeral: no_session_persistence,
                model: None,
                effort: Some("high".to_string()),
                permission_mode: Some(CLAUDE_DEFAULT_PERMISSION_MODE.to_string()),
                startup_mcp_config_json: build_claude_mcp_config_json(startup_mcp_servers),
                steering_content: steering_content.map(|s| s.to_string()),
                agent_identity: agent_identity.cloned(),
                tool_policy,
                last_cumulative_usage: None,
                conversation_bytes_total: 0,
                active_turn: None,
                subagent_emitter: None,
            }),
        });

        Ok((Self { inner }, event_rx))
    }

    pub(crate) async fn set_subagent_emitter(&self, emitter: Arc<dyn SubAgentEmitter>) {
        let mut state = self.inner.state.lock().await;
        state.subagent_emitter = Some(emitter);
    }

    pub fn command_handle(&self) -> ClaudeCommandHandle {
        ClaudeCommandHandle {
            inner: Arc::clone(&self.inner),
        }
    }

    pub async fn shutdown(self) {
        self.inner.shutdown().await;
    }
}

struct ActiveTurn {
    id: u64,
    cancel_tx: Option<oneshot::Sender<()>>,
}

struct ClaudeState {
    workspace_root: String,
    ssh_host: Option<String>,
    session_id: Option<String>,
    ephemeral: bool,
    model: Option<String>,
    effort: Option<String>,
    permission_mode: Option<String>,
    startup_mcp_config_json: Option<String>,
    steering_content: Option<String>,
    agent_identity: Option<AgentIdentity>,
    tool_policy: ToolPolicy,
    last_cumulative_usage: Option<Value>,
    conversation_bytes_total: u64,
    active_turn: Option<ActiveTurn>,
    subagent_emitter: Option<Arc<dyn SubAgentEmitter>>,
}

impl Default for ClaudeState {
    fn default() -> Self {
        Self {
            workspace_root: String::new(),
            ssh_host: None,
            session_id: None,
            ephemeral: false,
            model: None,
            effort: None,
            permission_mode: None,
            startup_mcp_config_json: None,
            steering_content: None,
            agent_identity: None,
            tool_policy: ToolPolicy::Unrestricted,
            last_cumulative_usage: None,
            conversation_bytes_total: 0,
            active_turn: None,
            subagent_emitter: None,
        }
    }
}

struct ClaudeInner {
    event_tx: mpsc::UnboundedSender<Value>,
    state: Mutex<ClaudeState>,
}

#[derive(Default)]
struct SegmentState {
    has_content: bool,
    segment_index: u64,
    awaiting_stream_start: bool,
    current_claude_message_id: Option<String>,
    pending_tool_uses: HashMap<u64, PendingClaudeToolUse>,
}

struct PendingClaudeToolUse {
    id: String,
    name: String,
    arguments: Value,
    partial_json: String,
    request_emitted: bool,
}

#[derive(Default)]
struct ClaudeStdoutSummary {
    streamed_text: String,
    streamed_reasoning: String,
    assistant_text: Option<String>,
    result_text: Option<String>,
    result_reasoning: Option<String>,
    model: Option<String>,
    session_id: Option<String>,
    /// Per-API-call usage from the most recent stream event or assistant message.
    usage: Option<Value>,
    /// Cumulative session usage from the `result` event (sum of all API calls).
    /// Kept separate from `usage` so we don't confuse per-call with cumulative.
    result_cumulative_usage: Option<Value>,
    /// Context window extracted from `result.modelUsage[model].contextWindow`.
    result_context_window: Option<u64>,
    errors: Vec<String>,
    tool_calls: Vec<ClaudeToolCall>,
    seen_tool_ids: HashSet<String>,
    tool_name_by_id: HashMap<String, String>,
    tool_call_by_id: HashMap<String, ClaudeToolCall>,
    tool_modify_preview_by_id: HashMap<String, ClaudeModifyPreview>,
    tool_io_bytes: u64,
    reasoning_bytes: u64,
    emitted_phase_count: u64,
    control_event: Option<ClaudeControlEvent>,
}

#[derive(Clone, Copy)]
enum ClaudeControlEvent {
    ConversationCompacted,
}

#[derive(Debug, Deserialize)]
struct ClaudeSystemFrame {
    #[serde(default)]
    model: Option<String>,
    subtype: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    task_id: Option<String>,
    #[serde(default)]
    task_type: Option<String>,
    #[serde(default)]
    tool_use_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ClaudeSystemEvent {
    Init,
    Status,
    CompactBoundary,
    TaskStarted,
    TaskProgress,
    TaskNotification,
    Unknown(String),
}

impl ClaudeSystemFrame {
    fn event(&self) -> ClaudeSystemEvent {
        match self.subtype.as_str() {
            "init" => ClaudeSystemEvent::Init,
            "status" => ClaudeSystemEvent::Status,
            "compact_boundary" => ClaudeSystemEvent::CompactBoundary,
            "task_started" => ClaudeSystemEvent::TaskStarted,
            "task_progress" => ClaudeSystemEvent::TaskProgress,
            "task_notification" => ClaudeSystemEvent::TaskNotification,
            other => ClaudeSystemEvent::Unknown(other.to_string()),
        }
    }
}

fn parse_claude_system_frame(value: &Value) -> Result<ClaudeSystemFrame, String> {
    serde_json::from_value::<ClaudeSystemFrame>(value.clone())
        .map_err(|err| format!("invalid Claude system frame: {err}; value={value}"))
}

#[doc(hidden)]
pub fn validate_system_frame(value: &Value) -> Result<(), String> {
    parse_claude_system_frame(value).map(|_| ())
}

impl ClaudeStdoutSummary {
    fn best_text(&self) -> String {
        if let Some(text) = self
            .result_text
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty())
        {
            return text.to_string();
        }

        if let Some(text) = self
            .assistant_text
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty())
        {
            return text.to_string();
        }

        self.streamed_text.trim().to_string()
    }

    fn best_reasoning(&self) -> Option<String> {
        if contains_non_whitespace(&self.streamed_reasoning) {
            return Some(self.streamed_reasoning.clone());
        }
        if let Some(reasoning) = self
            .result_reasoning
            .as_ref()
            .filter(|text| contains_non_whitespace(text))
        {
            return Some(reasoning.clone());
        }
        None
    }

    fn register_tool_call(&mut self, tool_call: ClaudeToolCall) -> bool {
        if tool_call.id.trim().is_empty() || self.seen_tool_ids.contains(&tool_call.id) {
            return false;
        }
        self.seen_tool_ids.insert(tool_call.id.clone());
        self.tool_name_by_id
            .insert(tool_call.id.clone(), tool_call.name.clone());
        self.tool_call_by_id
            .insert(tool_call.id.clone(), tool_call.clone());
        if let Some(preview) = claude_modify_preview(&tool_call.name, &tool_call.arguments) {
            self.tool_modify_preview_by_id
                .insert(tool_call.id.clone(), preview);
        }
        self.tool_io_bytes = self
            .tool_io_bytes
            .saturating_add(tool_call.name.len() as u64)
            .saturating_add(
                serde_json::to_string(&tool_call.arguments)
                    .expect("serde_json::Value is always serializable")
                    .len() as u64,
            );
        self.tool_calls.push(tool_call);
        true
    }

    fn error_message(&self) -> Option<String> {
        self.errors
            .iter()
            .map(|msg| msg.trim())
            .find(|msg| !msg.is_empty())
            .map(|msg| msg.to_string())
    }
}

#[derive(Clone)]
struct ClaudeToolCall {
    id: String,
    name: String,
    arguments: Value,
}

#[derive(Clone)]
struct ClaudeModifyPreview {
    file_path: String,
    before: String,
    after: String,
    lines_added: u64,
    lines_removed: u64,
}

struct ClaudeReplayToolExecution {
    tool_call_id: String,
    tool_name: String,
    success: bool,
    tool_result: Value,
    error: Option<String>,
}

struct ClaudePhaseEmission {
    text: String,
    reasoning: Option<String>,
    model: Option<String>,
    usage: Option<Value>,
    tool_calls: Vec<ClaudeToolCall>,
    tool_io_bytes: u64,
    reasoning_bytes: u64,
}

enum ClaudeHistoryReplayItem {
    Message(Value),
    ToolRequest(ClaudeToolCall),
    ToolExecutionCompleted(ClaudeReplayToolExecution),
}

struct ClaudeSessionReplay {
    items: Vec<ClaudeHistoryReplayItem>,
    last_cumulative_usage: Option<Value>,
    conversation_bytes_total: u64,
}

enum WaitResult {
    Exited(Result<std::process::ExitStatus, String>),
    Cancelled,
}

enum TurnOutcome {
    Completed {
        summary: ClaudeStdoutSummary,
        model_hint: Option<String>,
    },
    Cancelled {
        summary: ClaudeStdoutSummary,
    },
    Failed {
        summary: ClaudeStdoutSummary,
        error: String,
    },
}

struct RunTurnParams<'a> {
    message_id: &'a str,
    workspace_root: &'a str,
    ssh_host: Option<&'a str>,
    prompt: &'a str,
    images: &'a [ImageAttachment],
    session_id: Option<String>,
    ephemeral: bool,
    model: Option<String>,
    effort: Option<String>,
    permission_mode: Option<String>,
    startup_mcp_config_json: Option<String>,
    steering_content: Option<String>,
    agent_identity: Option<AgentIdentity>,
    tool_policy: ToolPolicy,
}

impl ClaudeInner {
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
            SessionCommand::ListModels => {
                this.emit_event(json!({
                    "kind": "ModelsList",
                    "data": {
                        "models": claude_known_models()
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

                    if let Some(effort_value) =
                        obj.get("effort").or_else(|| obj.get("reasoning_effort"))
                    {
                        state.effort = normalize_claude_effort(effort_value);
                    }

                    if let Some(permission_mode_value) = obj
                        .get("permission_mode")
                        .or_else(|| obj.get("permissionMode"))
                        .or_else(|| obj.get("approval_policy"))
                    {
                        if permission_mode_value.is_null() {
                            state.permission_mode = None;
                        } else if let Some(permission_mode) =
                            normalize_claude_permission_mode(permission_mode_value)
                        {
                            state.permission_mode = Some(permission_mode);
                        }
                    }
                }
                this.emit_settings().await;
                Ok(())
            }
        }
    }

    async fn start_turn(self: Arc<Self>, message: String, images: Option<Vec<ImageAttachment>>) {
        let images = images.unwrap_or_default();
        let input_bytes = estimate_turn_input_bytes(&message, &images);
        let (
            turn_id,
            workspace_root,
            ssh_host,
            session_id,
            ephemeral,
            model,
            effort,
            permission_mode,
            startup_mcp_config_json,
            steering_content,
            agent_identity,
            tool_policy,
            conversation_history_bytes,
            cancel_rx,
        ) = {
            let mut state = self.state.lock().await;
            if state.active_turn.is_some() {
                self.emit_error("Claude is still processing the previous turn.");
                return;
            }

            let turn_id = CLAUDE_TURN_COUNTER.fetch_add(1, Ordering::Relaxed);
            let (cancel_tx, cancel_rx) = oneshot::channel();
            state.active_turn = Some(ActiveTurn {
                id: turn_id,
                cancel_tx: Some(cancel_tx),
            });
            state.conversation_bytes_total =
                state.conversation_bytes_total.saturating_add(input_bytes);

            (
                turn_id,
                state.workspace_root.clone(),
                state.ssh_host.clone(),
                if state.ephemeral {
                    None
                } else {
                    state.session_id.clone()
                },
                state.ephemeral,
                state.model.clone(),
                state.effort.clone(),
                state.permission_mode.clone(),
                state.startup_mcp_config_json.clone(),
                state.steering_content.clone(),
                state.agent_identity.clone(),
                state.tool_policy.clone(),
                state.conversation_bytes_total,
                cancel_rx,
            )
        };

        let message_id = format!("claude-msg-{turn_id}");
        self.emit_typing_status(true);
        self.emit_stream_start(&message_id, model.clone());

        tokio::spawn(async move {
            let outcome = self
                .run_turn(
                    RunTurnParams {
                        message_id: &message_id,
                        workspace_root: &workspace_root,
                        ssh_host: ssh_host.as_deref(),
                        prompt: &message,
                        images: &images,
                        session_id,
                        ephemeral,
                        model,
                        effort,
                        permission_mode,
                        startup_mcp_config_json,
                        steering_content,
                        agent_identity,
                        tool_policy,
                    },
                    cancel_rx,
                )
                .await;

            match outcome {
                TurnOutcome::Completed {
                    summary,
                    model_hint,
                } => {
                    let mut summary = summary;
                    if !ephemeral && let Some(session_id) = summary.session_id.clone() {
                        self.set_session_id(turn_id, session_id.clone()).await;
                        self.emit_event(json!({
                            "kind": "SessionStarted",
                            "data": { "session_id": session_id }
                        }));
                    }

                    // result_cumulative_usage holds cumulative session totals
                    // from the `result` event — feed that into the cross-turn
                    // normalizer so the next process invocation can subtract it.
                    // Never fall back to summary.usage (per-API-call) here —
                    // mixing the two scales corrupts the differential math for
                    // subsequent turns.
                    let _ = self
                        .normalize_usage_for_turn(summary.result_cumulative_usage.clone())
                        .await;
                    let known_context_window = summary.result_context_window;
                    if !self
                        .emit_terminal_phase_or_placeholder(
                            &mut summary,
                            conversation_history_bytes,
                            known_context_window,
                            model_hint,
                        )
                        .await
                        && summary.emitted_phase_count == 0
                    {
                        self.emit_error("Claude returned no assistant output.");
                    }
                }
                TurnOutcome::Cancelled { summary } => {
                    let mut summary = summary;
                    let _ = self
                        .normalize_usage_for_turn(summary.result_cumulative_usage.clone())
                        .await;
                    let known_context_window = summary.result_context_window;
                    self.emit_terminal_phase_or_placeholder(
                        &mut summary,
                        conversation_history_bytes,
                        known_context_window,
                        None,
                    )
                    .await;
                    self.emit_operation_cancelled("Claude turn cancelled.");
                }
                TurnOutcome::Failed { summary, error } => {
                    let mut summary = summary;
                    let _ = self
                        .normalize_usage_for_turn(summary.result_cumulative_usage.take())
                        .await;
                    let known_context_window = summary.result_context_window;
                    let _ = self
                        .emit_terminal_phase_or_placeholder(
                            &mut summary,
                            conversation_history_bytes,
                            known_context_window,
                            None,
                        )
                        .await;
                    let detail = summary.error_message().unwrap_or(error);
                    self.emit_error(&detail);
                }
            }

            self.clear_active_turn(turn_id).await;
            self.emit_typing_status(false);
        });
    }

    async fn run_turn(
        self: &Arc<Self>,
        params: RunTurnParams<'_>,
        cancel_rx: oneshot::Receiver<()>,
    ) -> TurnOutcome {
        let RunTurnParams {
            message_id,
            workspace_root,
            ssh_host,
            prompt,
            images,
            session_id,
            ephemeral,
            model,
            effort,
            permission_mode,
            startup_mcp_config_json,
            steering_content,
            agent_identity,
            tool_policy,
        } = params;
        let effective_permission_mode = permission_mode
            .as_deref()
            .unwrap_or(CLAUDE_DEFAULT_PERMISSION_MODE);

        // Build the list of CLI arguments (excluding the binary name itself).
        let mut cli_args: Vec<String> = vec![
            "--print".to_string(),
            "--verbose".to_string(),
            "--output-format".to_string(),
            "stream-json".to_string(),
            "--input-format".to_string(),
            "stream-json".to_string(),
            "--include-partial-messages".to_string(),
            "--permission-mode".to_string(),
            effective_permission_mode.to_string(),
        ];

        if ephemeral {
            cli_args.push("--no-session-persistence".to_string());
        }

        if effective_permission_mode.eq_ignore_ascii_case("bypassPermissions") {
            cli_args.push("--dangerously-skip-permissions".to_string());
        }

        if let Some(model_name) = model.clone().and_then(|v| normalize_nonempty(&v)) {
            cli_args.push("--model".to_string());
            cli_args.push(model_name);
        }

        if let Some(effort_level) = effort.and_then(|v| normalize_nonempty(&v)) {
            cli_args.push("--effort".to_string());
            cli_args.push(effort_level);
        }

        if let Some(mcp_config_json) = startup_mcp_config_json
            && !mcp_config_json.trim().is_empty()
        {
            cli_args.push("--mcp-config".to_string());
            cli_args.push(mcp_config_json);
        }

        match tool_policy {
            ToolPolicy::Unrestricted => {}
            ToolPolicy::AllowList { tools } => {
                cli_args.push("--allowedTools".to_string());
                cli_args.extend(tools);
            }
            ToolPolicy::DenyList { tools } => {
                cli_args.push("--disallowedTools".to_string());
                cli_args.extend(tools);
            }
        }

        // Use --agents/--agent for agent definition instructions (first-class agent
        // identity that Claude CLI respects). Use --append-system-prompt only for
        // remaining steering (tool policy, workspace steering).
        if let Some(identity) = agent_identity {
            let agents_json = json!({
                &identity.id: {
                    "description": &identity.description,
                    "prompt": &identity.instructions,
                }
            });
            cli_args.push("--agents".to_string());
            cli_args.push(agents_json.to_string());
            cli_args.push("--agent".to_string());
            cli_args.push(identity.id.clone());
        }
        if let Some(steering) = steering_content
            && !steering.trim().is_empty()
        {
            cli_args.push("--append-system-prompt".to_string());
            cli_args.push(steering);
        }

        if !ephemeral && let Some(existing_session) = session_id {
            let trimmed = existing_session.trim();
            if !trimmed.is_empty() {
                cli_args.push("--resume".to_string());
                cli_args.push(trimmed.to_string());
            }
        }

        let mut child = if let Some(host) = ssh_host {
            match crate::remote::spawn_remote_process(
                host,
                "claude",
                &cli_args,
                Some(workspace_root),
            )
            .await
            {
                Ok(child) => child,
                Err(err) => {
                    return TurnOutcome::Failed {
                        summary: ClaudeStdoutSummary::default(),
                        error: format!("Failed to start Claude CLI over SSH: {err}"),
                    };
                }
            }
        } else {
            let mut cmd = Command::new("claude");
            for arg in &cli_args {
                cmd.arg(arg);
            }
            if let Some(path) = process_env::resolved_child_process_path() {
                cmd.env("PATH", path);
            }
            cmd.current_dir(workspace_root)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
            match cmd.spawn() {
                Ok(child) => child,
                Err(err) => {
                    return TurnOutcome::Failed {
                        summary: ClaudeStdoutSummary::default(),
                        error: format!("Failed to start Claude CLI: {err}"),
                    };
                }
            }
        };

        let mut stdin = match child.stdin.take() {
            Some(stdin) => stdin,
            None => {
                return TurnOutcome::Failed {
                    summary: ClaudeStdoutSummary::default(),
                    error: "Failed to capture Claude stdin".to_string(),
                };
            }
        };

        let input_message = build_stream_json_user_message(prompt, images);
        let input_payload = match serde_json::to_string(&input_message) {
            Ok(payload) => payload,
            Err(err) => {
                return TurnOutcome::Failed {
                    summary: ClaudeStdoutSummary::default(),
                    error: format!("Failed to encode Claude input payload: {err}"),
                };
            }
        };

        if let Err(err) = stdin.write_all(input_payload.as_bytes()).await {
            return TurnOutcome::Failed {
                summary: ClaudeStdoutSummary::default(),
                error: format!("Failed to write Claude input: {err}"),
            };
        }
        if let Err(err) = stdin.write_all(b"\n").await {
            return TurnOutcome::Failed {
                summary: ClaudeStdoutSummary::default(),
                error: format!("Failed to finalize Claude input: {err}"),
            };
        }
        drop(stdin);

        let stdout = match child.stdout.take() {
            Some(stdout) => stdout,
            None => {
                return TurnOutcome::Failed {
                    summary: ClaudeStdoutSummary::default(),
                    error: "Failed to capture Claude stdout".to_string(),
                };
            }
        };

        let stderr = match child.stderr.take() {
            Some(stderr) => stderr,
            None => {
                return TurnOutcome::Failed {
                    summary: ClaudeStdoutSummary::default(),
                    error: "Failed to capture Claude stderr".to_string(),
                };
            }
        };

        let subagent_emitter = {
            let state = self.state.lock().await;
            state.subagent_emitter.clone()
        };
        let stdout_task = tokio::spawn(read_claude_stdout(
            stdout,
            Arc::clone(self),
            message_id.to_string(),
            subagent_emitter,
        ));
        let stderr_task = tokio::spawn(read_claude_stderr(stderr));

        let mut cancel_rx = cancel_rx;
        let wait_result = tokio::select! {
            _ = &mut cancel_rx => WaitResult::Cancelled,
            status = child.wait() => {
                WaitResult::Exited(status.map_err(|err| format!("Failed to wait for Claude process: {err}")))
            }
        };

        if matches!(wait_result, WaitResult::Cancelled) {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }

        let (stdout_summary, _segment_state) = match stdout_task.await {
            Ok(pair) => pair,
            Err(err) => {
                return TurnOutcome::Failed {
                    summary: ClaudeStdoutSummary::default(),
                    error: format!("Failed to collect Claude stdout: {err}"),
                };
            }
        };

        let stderr_output = match stderr_task.await {
            Ok(stderr) => stderr,
            Err(err) => {
                return TurnOutcome::Failed {
                    summary: stdout_summary,
                    error: format!("Failed to collect Claude stderr: {err}"),
                };
            }
        };

        match wait_result {
            WaitResult::Cancelled => TurnOutcome::Cancelled {
                summary: stdout_summary,
            },
            WaitResult::Exited(Err(error)) => TurnOutcome::Failed {
                summary: stdout_summary,
                error,
            },
            WaitResult::Exited(Ok(status)) => {
                if status.success() {
                    if let Some(error) = stdout_summary.error_message() {
                        return TurnOutcome::Failed {
                            summary: stdout_summary,
                            error,
                        };
                    }

                    TurnOutcome::Completed {
                        summary: stdout_summary,
                        model_hint: model,
                    }
                } else {
                    let mut detail = stdout_summary.error_message();
                    if detail.is_none() {
                        let trimmed_stderr = stderr_output.trim();
                        if !trimmed_stderr.is_empty() {
                            detail = Some(trimmed_stderr.to_string());
                        }
                    }
                    let error =
                        detail.unwrap_or_else(|| format!("Claude exited with status {status}"));
                    TurnOutcome::Failed {
                        summary: stdout_summary,
                        error,
                    }
                }
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

    async fn add_conversation_bytes(&self, bytes: u64) {
        if bytes == 0 {
            return;
        }
        let mut state = self.state.lock().await;
        state.conversation_bytes_total = state.conversation_bytes_total.saturating_add(bytes);
    }

    async fn emit_settings(&self) {
        let (model, effort, permission_mode) = {
            let state = self.state.lock().await;
            (
                state.model.clone(),
                state.effort.clone(),
                state.permission_mode.clone(),
            )
        };

        self.emit_event(json!({
            "kind": "Settings",
            "data": {
                "model": model,
                "effort": effort,
                // Alias for existing settings UI consumers.
                "reasoning_effort": effort,
                "permission_mode": permission_mode,
            }
        }));
    }

    async fn list_sessions(&self) -> Result<(), String> {
        let (workspace_root, ssh_host) = {
            let state = self.state.lock().await;
            (state.workspace_root.clone(), state.ssh_host.clone())
        };

        let sessions = if let Some(host) = &ssh_host {
            list_claude_sessions_remote(host, &workspace_root).await?
        } else {
            list_claude_sessions(&workspace_root).await?
        };
        self.emit_event(json!({
            "kind": "SessionsList",
            "data": { "sessions": sessions }
        }));
        Ok(())
    }

    async fn resume_session(&self, session_id: String) -> Result<(), String> {
        let normalized = normalize_nonempty(&session_id).ok_or("Invalid session id")?;
        let (workspace_root, ssh_host) = {
            let mut state = self.state.lock().await;
            state.session_id = Some(normalized.clone());
            state.last_cumulative_usage = None;
            state.conversation_bytes_total = 0;
            (state.workspace_root.clone(), state.ssh_host.clone())
        };

        self.emit_event(json!({
            "kind": "SessionStarted",
            "data": { "session_id": &normalized }
        }));
        self.emit_event(json!({ "kind": "ConversationCleared" }));
        self.emit_event(json!({ "kind": "TypingStatusChanged", "data": false }));

        let replay = if let Some(host) = &ssh_host {
            load_claude_session_history_remote(host, &workspace_root, &normalized).await?
        } else {
            load_claude_session_history(&workspace_root, &normalized).await?
        };
        for item in replay.items {
            match item {
                ClaudeHistoryReplayItem::Message(message) => {
                    self.emit_event(json!({
                        "kind": "MessageAdded",
                        "data": message,
                    }));
                }
                ClaudeHistoryReplayItem::ToolRequest(tool_call) => {
                    self.emit_tool_request(&tool_call);
                }
                ClaudeHistoryReplayItem::ToolExecutionCompleted(completion) => {
                    self.emit_tool_execution_completed(
                        &completion.tool_call_id,
                        &completion.tool_name,
                        completion.success,
                        completion.tool_result,
                        completion.error,
                    );
                }
            }
        }

        let mut state = self.state.lock().await;
        state.last_cumulative_usage = replay.last_cumulative_usage;
        state.conversation_bytes_total = replay.conversation_bytes_total;
        Ok(())
    }

    async fn delete_session(&self, session_id: String) -> Result<(), String> {
        let normalized = normalize_nonempty(&session_id).ok_or("Invalid session id")?;
        let (workspace_root, ssh_host) = {
            let mut state = self.state.lock().await;
            if state.session_id.as_deref() == Some(normalized.as_str()) {
                state.session_id = None;
                state.last_cumulative_usage = None;
                state.conversation_bytes_total = 0;
            }
            (state.workspace_root.clone(), state.ssh_host.clone())
        };

        if let Some(host) = &ssh_host {
            delete_claude_session_remote(host, &workspace_root, &normalized).await?;
        } else {
            let session_file = claude_session_file_path(&workspace_root, &normalized)?;
            if let Err(err) = tokio_fs::remove_file(&session_file).await
                && err.kind() != std::io::ErrorKind::NotFound
            {
                return Err(format!(
                    "Failed to delete Claude session '{}': {err}",
                    session_file.display()
                ));
            }
        }
        self.list_sessions().await?;
        Ok(())
    }

    fn emit_tool_request(&self, tool_call: &ClaudeToolCall) {
        if claude_is_todo_write_tool_name(&tool_call.name)
            && let Some(task_update) = claude_task_update_from_todo_write(&tool_call.arguments)
        {
            self.emit_event(task_update);
        }
        self.emit_event(json!({
            "kind": "ToolRequest",
            "data": {
                "tool_call_id": tool_call.id,
                "tool_name": tool_call.name,
                "tool_type": claude_tool_request_type(&tool_call.name, &tool_call.arguments),
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

    async fn shutdown(&self) {
        self.cancel_active_turn().await;
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
                "agent": CLAUDE_AGENT_NAME,
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

    fn emit_stream_reasoning_delta(&self, message_id: &str, text: &str) {
        if text.is_empty() {
            return;
        }
        self.emit_event(json!({
            "kind": "StreamReasoningDelta",
            "data": {
                "message_id": message_id,
                "text": text,
            }
        }));
    }

    fn emit_system_message(&self, content: &str) {
        self.emit_event(json!({
            "kind": "MessageAdded",
            "data": {
                "timestamp": unix_now_ms(),
                "sender": "System",
                "content": content,
                "tool_calls": [],
                "images": [],
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
        context_breakdown: Option<Value>,
    ) {
        let model_info = model
            .filter(|m| !m.trim().is_empty())
            .map(|m| json!({ "model": m }))
            .unwrap_or(Value::Null);
        let usage_value = usage.unwrap_or(Value::Null);
        let reasoning_value = reasoning
            .filter(|value| !value.trim().is_empty())
            .map(|text| json!({ "text": text }))
            .unwrap_or(Value::Null);
        let context_breakdown_value = context_breakdown.unwrap_or(Value::Null);

        self.emit_event(json!({
            "kind": "StreamEnd",
            "data": {
                "message": {
                    "timestamp": unix_now_ms(),
                    "sender": { "Assistant": { "agent": CLAUDE_AGENT_NAME } },
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

    fn emit_placeholder_stream_end(&self, model: Option<String>, context_breakdown: Option<Value>) {
        self.emit_stream_end(
            String::new(),
            model,
            None,
            None,
            Vec::new(),
            context_breakdown,
        );
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

    async fn normalize_usage_for_turn(&self, usage: Option<Value>) -> Option<Value> {
        let cumulative_usage = usage?;

        let mut state = self.state.lock().await;
        let per_turn =
            derive_turn_token_usage(&cumulative_usage, state.last_cumulative_usage.as_ref());
        state.last_cumulative_usage = Some(cumulative_usage);
        per_turn
    }

    async fn emit_terminal_phase_or_placeholder(
        &self,
        summary: &mut ClaudeStdoutSummary,
        conversation_history_bytes: u64,
        known_context_window: Option<u64>,
        model_hint: Option<String>,
    ) -> bool {
        if let Some(phase) = take_phase_emission(summary) {
            let text = phase.text;
            let selected_model = phase.model.clone().or(model_hint);
            let tool_calls = phase
                .tool_calls
                .iter()
                .map(|tool| {
                    json!({
                        "id": tool.id,
                        "name": tool.name,
                        "arguments": tool.arguments,
                    })
                })
                .collect::<Vec<_>>();
            let context_breakdown = estimate_context_breakdown(
                phase.usage.as_ref(),
                conversation_history_bytes,
                phase.tool_io_bytes,
                phase.reasoning_bytes,
                known_context_window,
                selected_model.as_deref(),
            );
            if !text.is_empty() {
                self.add_conversation_bytes(text.len() as u64).await;
            }
            self.emit_stream_end(
                text,
                selected_model,
                phase.usage,
                phase.reasoning,
                tool_calls,
                Some(context_breakdown),
            );
            for tool_call in &phase.tool_calls {
                self.emit_tool_request(tool_call);
            }
            return true;
        }

        if let Some(control_event) = summary.control_event {
            if summary.emitted_phase_count == 0 {
                let selected_model = summary.model.clone().or(model_hint);
                let context_breakdown = estimate_context_breakdown(
                    None,
                    conversation_history_bytes,
                    summary.tool_io_bytes,
                    summary.reasoning_bytes,
                    known_context_window,
                    selected_model.as_deref(),
                );
                match control_event {
                    ClaudeControlEvent::ConversationCompacted => {
                        self.emit_system_message(CLAUDE_CONVERSATION_COMPACTED_NOTICE);
                    }
                }
                self.emit_placeholder_stream_end(selected_model, Some(context_breakdown));
            }
            return true;
        }

        if summary.emitted_phase_count == 0 {
            let selected_model = summary.model.clone().or(model_hint);
            let context_breakdown = estimate_context_breakdown(
                None,
                conversation_history_bytes,
                summary.tool_io_bytes,
                summary.reasoning_bytes,
                known_context_window,
                selected_model.as_deref(),
            );
            self.emit_placeholder_stream_end(selected_model, Some(context_breakdown));
        }

        false
    }
}

fn build_claude_mcp_config_json(startup_mcp_servers: &[StartupMcpServer]) -> Option<String> {
    if startup_mcp_servers.is_empty() {
        return None;
    }

    let mut servers = serde_json::Map::new();
    for server in startup_mcp_servers {
        let name = server.name.trim();
        if name.is_empty() {
            continue;
        }
        match &server.transport {
            StartupMcpTransport::Http { url, headers, .. } => {
                let trimmed_url = url.trim();
                if trimmed_url.is_empty() {
                    continue;
                }
                let mut config = serde_json::Map::new();
                config.insert("type".to_string(), Value::String("http".to_string()));
                config.insert("url".to_string(), Value::String(trimmed_url.to_string()));
                if !headers.is_empty() {
                    config.insert(
                        "headers".to_string(),
                        serde_json::to_value(headers)
                            .expect("HashMap<String, String> is always serializable"),
                    );
                }
                servers.insert(name.to_string(), Value::Object(config));
            }
            StartupMcpTransport::Stdio { command, args, env } => {
                let trimmed_command = command.trim();
                if trimmed_command.is_empty() {
                    continue;
                }
                let mut config = serde_json::Map::new();
                config.insert("type".to_string(), Value::String("stdio".to_string()));
                config.insert(
                    "command".to_string(),
                    Value::String(trimmed_command.to_string()),
                );
                config.insert(
                    "args".to_string(),
                    serde_json::to_value(args).expect("Vec<String> is always serializable"),
                );
                config.insert(
                    "env".to_string(),
                    serde_json::to_value(env)
                        .expect("HashMap<String, String> is always serializable"),
                );
                servers.insert(name.to_string(), Value::Object(config));
            }
        }
    }

    if servers.is_empty() {
        return None;
    }

    Some(
        serde_json::json!({
            "mcpServers": servers,
        })
        .to_string(),
    )
}

/// Tool names that indicate a sub-agent spawn in Claude Code.
const SUBAGENT_TOOL_NAMES: &[&str] = &["Task", "Agent"];

fn is_subagent_tool_name(name: &str) -> bool {
    SUBAGENT_TOOL_NAMES.contains(&name)
}

/// Extract `parent_tool_use_id` from a Claude Code stream-json event.
/// Returns `None` for root-level events, `Some(id)` for sub-agent events.
fn extract_parent_tool_use_id(value: &Value) -> Option<&str> {
    value
        .get("parent_tool_use_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

/// Extract sub-agent spawn info from a tool_use content block.
fn extract_spawn_description(input: Option<&Value>) -> String {
    let Some(input) = input else {
        return String::new();
    };

    for key in ["prompt", "task", "instruction", "message", "description"] {
        if let Some(text) = input.get(key).and_then(extract_reasoning_text) {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }

    String::new()
}

fn extract_spawn_info(block: &Value) -> Option<(String, String, String, String)> {
    let name = block.get("name").and_then(Value::as_str)?;
    if !is_subagent_tool_name(name) {
        return None;
    }
    let id = block.get("id").and_then(Value::as_str)?.to_string();
    let input = block.get("input");
    let description = extract_spawn_description(input);
    let agent_type = input
        .and_then(|i| i.get("subagent_type").or(i.get("name")))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    // Prefer the short "description" field (3-5 word label) as the display name,
    // falling back to subagent_type or the tool name.
    let agent_name = input
        .and_then(|i| i.get("description"))
        .and_then(Value::as_str)
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            if !agent_type.is_empty() {
                agent_type.clone()
            } else {
                name.to_string()
            }
        });
    Some((id, agent_name, description, agent_type))
}

async fn read_claude_stdout(
    stdout: ChildStdout,
    inner: Arc<ClaudeInner>,
    base_message_id: String,
    subagent_emitter: Option<Arc<dyn SubAgentEmitter>>,
) -> (ClaudeStdoutSummary, SegmentState) {
    let mut summary = ClaudeStdoutSummary::default();
    let mut segment = SegmentState::default();
    let mut current_message_id = base_message_id.clone();
    let mut lines = BufReader::new(stdout).lines();

    // Sub-agent tracking: tool_use_id → SubAgentStream
    let mut subagent_streams: HashMap<String, SubAgentStream> = HashMap::new();
    // Root pending Task/Agent tool args by content block index so we can
    // surface the initial task prompt as soon as input_json_delta is complete.
    let mut pending_subagent_prompts: HashMap<u64, PendingSubAgentPrompt> = HashMap::new();

    while let Ok(Some(line)) = lines.next_line().await {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let value = match serde_json::from_str::<Value>(trimmed) {
            Ok(value) => value,
            Err(_) => {
                tracing::warn!("Non-JSON line from Claude CLI: {trimmed}");
                continue;
            }
        };

        if let Some(ref emitter) = subagent_emitter {
            detect_subagent_task_system_spawns(&value, emitter.as_ref(), &mut subagent_streams)
                .await;
        }

        // Check if this event belongs to a sub-agent
        if let Some(parent_id) = extract_parent_tool_use_id(&value) {
            if let Some(stream) = subagent_streams.get_mut(parent_id) {
                consume_subagent_event(stream, &value);
            }
            // If we don't have a stream for this parent_id, the spawn hasn't been
            // detected yet (or emitter is None). Drop the event silently — it would
            // have contaminated the root stream otherwise.
            continue;
        }

        // Root-level event: check if it contains a sub-agent spawn (tool_use)
        if let Some(ref emitter) = subagent_emitter {
            detect_subagent_spawns(
                &value,
                emitter.as_ref(),
                &mut subagent_streams,
                &mut pending_subagent_prompts,
            )
            .await;
        } else {
            let event_type = value.get("type").and_then(Value::as_str).unwrap_or("?");
            tracing::trace!(
                "read_claude_stdout: no subagent_emitter set, skipping spawn detection for event type={event_type}"
            );
        }

        consume_claude_stream_value(
            &value,
            &mut summary,
            &mut segment,
            &inner,
            &base_message_id,
            &mut current_message_id,
        );

        // Check if root received a tool_result for a sub-agent tool
        if subagent_emitter.is_some() {
            detect_subagent_completions(&value, &mut subagent_streams).await;
        }
    }

    // Flush any remaining sub-agent streams
    for (_tool_use_id, mut stream) in subagent_streams.drain() {
        flush_pending_tool_uses(&mut stream.summary, &mut stream.segment);
        if phase_has_pending_output(&stream.summary, &stream.segment) {
            close_current_phase(&mut stream.summary, &mut stream.segment, &stream.inner);
        }
    }

    flush_pending_tool_uses(&mut summary, &mut segment);

    (summary, segment)
}

fn consume_subagent_event(stream: &mut SubAgentStream, value: &Value) {
    let mut sa_message_id = stream.message_id.clone();
    consume_claude_stream_value(
        value,
        &mut stream.summary,
        &mut stream.segment,
        &stream.inner,
        &stream.message_id,
        &mut sa_message_id,
    );
    stream.message_id = sa_message_id;
}

fn emit_subagent_task_prompt_if_needed(stream: &mut SubAgentStream, description: &str) {
    let trimmed = description.trim();
    if stream.has_explicit_task_prompt || trimmed.is_empty() {
        return;
    }
    stream.has_explicit_task_prompt = true;
    stream.inner.emit_event(json!({
        "kind": "MessageAdded",
        "data": {
            "timestamp": unix_now_ms(),
            "sender": "User",
            "content": trimmed,
            "tool_calls": [],
            "images": [],
        }
    }));
}

async fn ensure_subagent_stream(
    emitter: &dyn SubAgentEmitter,
    streams: &mut HashMap<String, SubAgentStream>,
    tool_use_id: String,
    name: String,
    description: String,
    agent_type: String,
    session_id_hint: Option<protocol::SessionId>,
) {
    if streams.contains_key(&tool_use_id) {
        return;
    }

    tracing::info!(
        "registering Claude sub-agent stream tool_use_id={tool_use_id} name={name} agent_type={agent_type}"
    );
    let handle = emitter
        .on_subagent_spawned(
            tool_use_id.clone(),
            name,
            description,
            agent_type,
            session_id_hint,
        )
        .await;
    let (raw_event_tx, raw_event_rx) = mpsc::unbounded_channel();
    spawn_claude_subagent_event_bridge(raw_event_rx, handle.event_tx.clone());

    // Create a ClaudeInner that routes events to the sub-agent's channel.
    let sa_inner = Arc::new(ClaudeInner {
        event_tx: raw_event_tx,
        state: Mutex::new(ClaudeState::default()),
    });
    let sa_message_id = format!("subagent-{}", tool_use_id);

    streams.insert(
        tool_use_id,
        SubAgentStream {
            summary: ClaudeStdoutSummary::default(),
            segment: SegmentState {
                awaiting_stream_start: true,
                ..SegmentState::default()
            },
            message_id: sa_message_id,
            has_explicit_task_prompt: false,
            inner: sa_inner,
        },
    );
}

async fn detect_subagent_task_system_spawns(
    value: &Value,
    emitter: &dyn SubAgentEmitter,
    streams: &mut HashMap<String, SubAgentStream>,
) {
    if value.get("type").and_then(Value::as_str) != Some("system") {
        return;
    }

    let Ok(system) = parse_claude_system_frame(value) else {
        return;
    };

    if system.event() != ClaudeSystemEvent::TaskStarted {
        return;
    }

    let task_type = system
        .task_type
        .as_deref()
        .and_then(normalize_nonempty)
        .unwrap_or_default();
    if task_type != "local_agent" {
        return;
    }

    let Some(tool_use_id) = system.tool_use_id.as_deref().and_then(normalize_nonempty) else {
        tracing::debug!("ignoring Claude task_started without tool_use_id");
        return;
    };

    let task_name = system.description.as_deref().and_then(normalize_nonempty);
    let prompt = system.prompt.as_deref().and_then(normalize_nonempty);
    let name = task_name.clone().unwrap_or_else(|| "Agent".to_string());
    let description = prompt
        .clone()
        .or_else(|| task_name.clone())
        .unwrap_or_else(|| name.clone());

    ensure_subagent_stream(
        emitter,
        streams,
        tool_use_id.clone(),
        name,
        description,
        task_type,
        None,
    )
    .await;

    if let Some(stream) = streams.get_mut(&tool_use_id)
        && let Some(initial_prompt) = prompt.as_deref().or(task_name.as_deref())
    {
        emit_subagent_task_prompt_if_needed(stream, initial_prompt);
    }
}

fn normalize_stream_event_for_spawn(value: &Value) -> Option<Value> {
    let event_type = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if event_type == "stream_event" {
        let event = value.get("event")?;
        if event.is_object() {
            return Some(event.clone());
        }
        let event_name = event.as_str()?;
        if is_stream_event_type(event_name) {
            return Some(merge_data_with_type(
                event_name,
                value.get("data").unwrap_or(&Value::Null),
            ));
        }
        return None;
    }
    if is_stream_event_type(event_type) {
        return Some(value.clone());
    }
    None
}

fn track_pending_subagent_prompt_event(
    value: &Value,
    streams: &mut HashMap<String, SubAgentStream>,
    pending_prompts: &mut HashMap<u64, PendingSubAgentPrompt>,
) {
    fn maybe_emit_prompt_from_pending(
        pending: &PendingSubAgentPrompt,
        streams: &mut HashMap<String, SubAgentStream>,
    ) {
        let Ok(parsed) = serde_json::from_str::<Value>(&pending.partial_json) else {
            return;
        };
        let description = extract_spawn_description(Some(&parsed));
        let Some(stream) = streams.get_mut(&pending.tool_use_id) else {
            return;
        };
        emit_subagent_task_prompt_if_needed(stream, &description);
    }

    let Some(event) = normalize_stream_event_for_spawn(value) else {
        return;
    };
    let event_type = event
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match event_type {
        "content_block_start" => {
            let Some(index) = content_block_index(&event) else {
                return;
            };
            let Some(block) = event.get("content_block") else {
                return;
            };
            let Some((tool_use_id, _name, description, _agent_type)) = extract_spawn_info(block)
            else {
                return;
            };
            pending_prompts.insert(
                index,
                PendingSubAgentPrompt {
                    tool_use_id: tool_use_id.clone(),
                    partial_json: String::new(),
                },
            );
            if let Some(stream) = streams.get_mut(&tool_use_id) {
                emit_subagent_task_prompt_if_needed(stream, &description);
            }
        }
        "content_block_delta" => {
            let Some(index) = content_block_index(&event) else {
                return;
            };
            let Some(delta) = event.get("delta") else {
                return;
            };
            if delta.get("type").and_then(Value::as_str) != Some("input_json_delta") {
                return;
            }
            let Some(partial) = extract_tool_json_delta(delta) else {
                return;
            };
            let Some(pending) = pending_prompts.get_mut(&index) else {
                return;
            };
            pending.partial_json.push_str(partial);
            maybe_emit_prompt_from_pending(pending, streams);
        }
        "content_block_stop" => {
            let Some(index) = content_block_index(&event) else {
                return;
            };
            if let Some(pending) = pending_prompts.remove(&index) {
                maybe_emit_prompt_from_pending(&pending, streams);
            }
        }
        "message_stop" => {
            for pending in pending_prompts.values() {
                maybe_emit_prompt_from_pending(pending, streams);
            }
            pending_prompts.clear();
        }
        _ => {}
    }
}

/// Scan a root-level event for tool_use blocks that spawn sub-agents.
async fn detect_subagent_spawns(
    value: &Value,
    emitter: &dyn SubAgentEmitter,
    streams: &mut HashMap<String, SubAgentStream>,
    pending_prompts: &mut HashMap<u64, PendingSubAgentPrompt>,
) {
    track_pending_subagent_prompt_event(value, streams, pending_prompts);

    // Sub-agent spawns appear as tool_use content blocks in assistant messages
    // or as content_block_start events in the stream.
    let blocks = collect_tool_use_blocks(value);
    if blocks.is_empty() {
        let event_type = value.get("type").and_then(Value::as_str).unwrap_or("?");
        tracing::trace!(
            "detect_subagent_spawns: no tool_use blocks found in event type={event_type}"
        );
    }
    for block in blocks {
        let block_name = block.get("name").and_then(|v| v.as_str()).unwrap_or("?");
        let block_id = block.get("id").and_then(|v| v.as_str()).unwrap_or("?");
        tracing::info!(
            "detect_subagent_spawns: found tool_use block: name={block_name} id={block_id}"
        );
        if let Some((tool_use_id, name, description, agent_type)) = extract_spawn_info(&block) {
            ensure_subagent_stream(
                emitter,
                streams,
                tool_use_id.clone(),
                name,
                description.clone(),
                agent_type,
                None,
            )
            .await;
            if let Some(stream) = streams.get_mut(&tool_use_id) {
                emit_subagent_task_prompt_if_needed(stream, &description);
            }
        }
    }
}

/// Detect tool_result events for sub-agent tools and finalize the sub-agent.
async fn detect_subagent_completions(value: &Value, streams: &mut HashMap<String, SubAgentStream>) {
    // tool_result appears in "user" type messages with content blocks
    let msg_type = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if msg_type != "user" {
        return;
    }
    let content = match value
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array)
    {
        Some(c) => c,
        None => return,
    };
    for block in content {
        let block_type = block
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if block_type != "tool_result" {
            continue;
        }
        let tool_use_id = match block.get("tool_use_id").and_then(Value::as_str) {
            Some(id) => id,
            None => continue,
        };
        if let Some(mut stream) = streams.remove(tool_use_id) {
            flush_pending_tool_uses(&mut stream.summary, &mut stream.segment);
            if phase_has_pending_output(&stream.summary, &stream.segment) {
                close_current_phase(&mut stream.summary, &mut stream.segment, &stream.inner);
            }
        }
    }
}

/// Collect tool_use blocks from various event shapes.
fn collect_tool_use_blocks(value: &Value) -> Vec<Value> {
    let mut blocks = Vec::new();

    // From stream_event content_block_start
    if let Some(event) = normalize_stream_event_for_spawn(value) {
        let inner_type = event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if inner_type == "content_block_start"
            && let Some(block) = event.get("content_block")
            && block.get("type").and_then(Value::as_str) == Some("tool_use")
        {
            blocks.push(block.clone());
        }
    }

    // From "assistant" messages with content array
    let event_type = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if event_type == "assistant"
        && let Some(content) = value
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(Value::as_array)
    {
        for block in content {
            if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                blocks.push(block.clone());
            }
        }
    }

    blocks
}

async fn read_claude_stderr(stderr: tokio::process::ChildStderr) -> String {
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

fn consume_claude_stream_value(
    value: &Value,
    summary: &mut ClaudeStdoutSummary,
    segment: &mut SegmentState,
    inner: &ClaudeInner,
    base_message_id: &str,
    current_message_id: &mut String,
) {
    if let Some(session_id) = value.get("session_id").and_then(Value::as_str) {
        let is_new_session = summary.session_id.as_deref() != Some(session_id);
        summary.session_id = Some(session_id.to_string());
        if is_new_session {
            inner.emit_event(json!({
                "kind": "SessionStarted",
                "data": { "session_id": session_id }
            }));
        }
    }

    let message_type = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();

    match message_type {
        "event" => {
            let event_name = value
                .get("event")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if event_name.is_empty() {
                return;
            }
            let data = value.get("data").unwrap_or(&Value::Null);
            consume_named_claude_event(
                event_name,
                data,
                summary,
                segment,
                inner,
                base_message_id,
                current_message_id,
            );
        }
        "system" => {
            let system = parse_claude_system_frame(value).unwrap_or_else(|err| panic!("{err}"));
            if let Some(model) = system.model.as_ref() {
                summary.model = Some(model.clone());
            }
            match system.event() {
                ClaudeSystemEvent::Init => {}
                ClaudeSystemEvent::Status => {}
                ClaudeSystemEvent::CompactBoundary => {
                    summary.control_event = Some(ClaudeControlEvent::ConversationCompacted);
                }
                ClaudeSystemEvent::TaskStarted
                | ClaudeSystemEvent::TaskProgress
                | ClaudeSystemEvent::TaskNotification => {
                    let _ = (&system.task_id, &system.status, &system.summary);
                }
                ClaudeSystemEvent::Unknown(subtype) => {
                    tracing::warn!("Ignoring unrecognized Claude system subtype: {subtype}");
                }
            }
        }
        "assistant" => {
            consume_assistant_message(
                value,
                summary,
                segment,
                inner,
                base_message_id,
                current_message_id,
            );
        }
        "user" => {
            consume_user_tool_result(value, summary, segment, inner);
        }
        "result" => {
            if let Some(session_id) = value.get("session_id").and_then(Value::as_str) {
                summary.session_id = Some(session_id.to_string());
            }
            if let Some(text) = value.get("result").and_then(Value::as_str) {
                summary.result_text = Some(text.to_string());
            }
            if let Some(reasoning) = extract_reasoning_from_result(value) {
                summary.result_reasoning = Some(reasoning.clone());
                append_reasoning_text(summary, &reasoning, true);
            }
            // result.usage is cumulative across all API calls in the session.
            // Store it separately — do NOT overwrite summary.usage which holds
            // per-API-call values from stream events / assistant messages.
            if let Some(usage) = parse_token_usage(value.get("usage")) {
                summary.result_cumulative_usage = Some(usage);
            }
            // Extract contextWindow from result.modelUsage[model].contextWindow.
            // This is the only place Claude Code reports the actual context window.
            if let Some(model_usage) = value.get("modelUsage").and_then(Value::as_object) {
                let preferred_model = value
                    .get("model")
                    .and_then(Value::as_str)
                    .or(summary.model.as_deref());
                summary.result_context_window =
                    extract_context_window_from_model_usage(model_usage, preferred_model);
            }

            let is_error = value
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if is_error {
                if let Some(message) = extract_result_error(value) {
                    summary.errors.push(message);
                } else if let Some(result) = value.get("result").and_then(Value::as_str) {
                    let trimmed = result.trim();
                    if !trimmed.is_empty() {
                        summary.errors.push(trimmed.to_string());
                    }
                }
            }
        }
        "stream_event" => {
            let Some(event) = value.get("event") else {
                return;
            };
            if event.is_object() {
                consume_stream_event(
                    event,
                    summary,
                    segment,
                    inner,
                    base_message_id,
                    current_message_id,
                );
                return;
            }
            if let Some(event_name) = event.as_str() {
                let data = value.get("data").unwrap_or(&Value::Null);
                consume_named_claude_event(
                    event_name,
                    data,
                    summary,
                    segment,
                    inner,
                    base_message_id,
                    current_message_id,
                );
            }
        }
        _ if is_stream_event_type(message_type) => {
            consume_stream_event(
                value,
                summary,
                segment,
                inner,
                base_message_id,
                current_message_id,
            );
        }
        _ => {
            if let Some(error) = extract_result_error(value) {
                summary.errors.push(error);
            }
        }
    }
}

fn consume_named_claude_event(
    event_name: &str,
    data: &Value,
    summary: &mut ClaudeStdoutSummary,
    segment: &mut SegmentState,
    inner: &ClaudeInner,
    base_message_id: &str,
    current_message_id: &mut String,
) {
    if event_name.trim().is_empty() || event_name.eq_ignore_ascii_case("event") {
        return;
    }

    let payload = merge_data_with_type(event_name, data);
    if is_stream_event_type(event_name) {
        consume_stream_event(
            &payload,
            summary,
            segment,
            inner,
            base_message_id,
            current_message_id,
        );
    } else {
        consume_claude_stream_value(
            &payload,
            summary,
            segment,
            inner,
            base_message_id,
            current_message_id,
        );
    }
}

fn merge_data_with_type(message_type: &str, data: &Value) -> Value {
    if let Some(obj) = data.as_object() {
        let mut merged = obj.clone();
        merged
            .entry("type".to_string())
            .or_insert_with(|| Value::String(message_type.to_string()));
        Value::Object(merged)
    } else {
        json!({
            "type": message_type,
            "data": data,
        })
    }
}

fn is_stream_event_type(event_type: &str) -> bool {
    matches!(
        event_type,
        "message_start"
            | "message_delta"
            | "message_stop"
            | "content_block_start"
            | "content_block_delta"
            | "content_block_stop"
    ) || is_reasoning_marker(event_type)
}

fn consume_assistant_message(
    value: &Value,
    summary: &mut ClaudeStdoutSummary,
    segment: &mut SegmentState,
    inner: &ClaudeInner,
    base_message_id: &str,
    current_message_id: &mut String,
) {
    if let Some(session_id) = value.get("session_id").and_then(Value::as_str) {
        summary.session_id = Some(session_id.to_string());
    }

    let Some(message) = value.get("message") else {
        return;
    };

    let next_model = message
        .get("model")
        .and_then(Value::as_str)
        .map(|model| model.to_string());
    let next_message_id = extract_claude_message_id(message);
    let next_text = extract_text_from_message(message);
    let next_reasoning = extract_reasoning_from_message(message);
    let next_usage = parse_token_usage(message.get("usage"));
    let next_tool_calls = extract_tool_calls_from_message(message);
    let has_payload = next_text
        .as_deref()
        .is_some_and(|text| !text.trim().is_empty())
        || next_reasoning
            .as_deref()
            .is_some_and(|text| !text.trim().is_empty())
        || !next_tool_calls.is_empty();

    let starts_new_phase = next_message_id
        .as_ref()
        .zip(segment.current_claude_message_id.as_ref())
        .is_some_and(|(next, current)| next != current);
    if has_payload && starts_new_phase && phase_has_pending_output(summary, segment) {
        close_current_phase(summary, segment, inner);
    }

    let is_duplicate = next_message_id
        .as_ref()
        .zip(segment.current_claude_message_id.as_ref())
        .is_some_and(|(next, current)| next == current);

    if has_payload && !is_duplicate {
        maybe_emit_next_stream_start(
            segment,
            inner,
            base_message_id,
            current_message_id,
            next_model.clone().or_else(|| summary.model.clone()),
        );
    }

    if let Some(model) = next_model {
        summary.model = Some(model);
    }
    if let Some(message_id) = next_message_id {
        segment.current_claude_message_id = Some(message_id);
    }

    if let Some(text) = next_text {
        summary.assistant_text = Some(text);
        segment.has_content = true;
    }

    if let Some(reasoning) = next_reasoning {
        append_reasoning_text(summary, &reasoning, true);
        segment.has_content = true;
    }

    if let Some(usage) = next_usage {
        summary.usage = Some(usage);
    }

    for tool_call in next_tool_calls {
        if summary.register_tool_call(tool_call) {
            segment.has_content = true;
        }
    }
}

fn consume_user_tool_result(
    value: &Value,
    summary: &mut ClaudeStdoutSummary,
    segment: &mut SegmentState,
    inner: &ClaudeInner,
) {
    let Some(message) = value.get("message") else {
        return;
    };
    let Some(content) = message.get("content").and_then(Value::as_array) else {
        return;
    };

    if phase_has_pending_output(summary, segment) {
        close_current_phase(summary, segment, inner);
    }

    for block in content {
        if block.get("type").and_then(Value::as_str) != Some("tool_result") {
            continue;
        }

        let result_text = extract_tool_result_content(block);
        summary.tool_io_bytes = summary
            .tool_io_bytes
            .saturating_add(result_text.len() as u64)
            .saturating_add(serde_json::to_string(block).unwrap_or_default().len() as u64);
    }

    let completions = extract_tool_result_events_from_message(
        message,
        &summary.tool_name_by_id,
        &summary.tool_call_by_id,
    );
    for completion in completions {
        inner.emit_tool_execution_completed(
            &completion.tool_call_id,
            &completion.tool_name,
            completion.success,
            completion.tool_result,
            completion.error,
        );
    }
}

fn has_meaningful_tool_arguments(arguments: &Value) -> bool {
    match arguments {
        Value::Null => false,
        Value::Object(map) => !map.is_empty(),
        Value::Array(values) => !values.is_empty(),
        Value::String(text) => !text.trim().is_empty(),
        _ => true,
    }
}

fn extract_claude_message_id(message: &Value) -> Option<String> {
    message
        .get("id")
        .and_then(Value::as_str)
        .and_then(normalize_nonempty)
}

fn phase_has_pending_output(summary: &ClaudeStdoutSummary, segment: &SegmentState) -> bool {
    segment.has_content
        || !segment.pending_tool_uses.is_empty()
        || !summary.tool_calls.is_empty()
        || !summary.streamed_text.trim().is_empty()
        || !summary.streamed_reasoning.trim().is_empty()
        || summary
            .assistant_text
            .as_deref()
            .is_some_and(|text| !text.trim().is_empty())
        || summary
            .result_text
            .as_deref()
            .is_some_and(|text| !text.trim().is_empty())
        || summary
            .result_reasoning
            .as_deref()
            .is_some_and(|text| !text.trim().is_empty())
}

fn maybe_emit_next_stream_start(
    segment: &mut SegmentState,
    inner: &ClaudeInner,
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

fn phase_usage_for_emission(summary: &mut ClaudeStdoutSummary) -> Option<Value> {
    // Return the raw usage from stream events as-is.  These values are
    // session-cumulative (they grow across API calls within a process
    // invocation), which is exactly what the token badge and context
    // breakdown need — they should reflect what the latest API call
    // actually consumed, not a delta from the previous call.
    summary.usage.clone()
}

fn take_phase_emission(summary: &mut ClaudeStdoutSummary) -> Option<ClaudePhaseEmission> {
    let text = {
        let streamed = summary.streamed_text.trim();
        if !streamed.is_empty() {
            streamed.to_string()
        } else {
            summary.best_text()
        }
    };
    let reasoning = summary.best_reasoning();
    let tool_calls = summary.tool_calls.clone();
    let has_payload = !text.is_empty()
        || reasoning
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty())
        || !tool_calls.is_empty();
    if !has_payload {
        return None;
    }

    let emission = ClaudePhaseEmission {
        text,
        reasoning,
        model: summary.model.clone(),
        usage: phase_usage_for_emission(summary),
        tool_calls,
        tool_io_bytes: summary.tool_io_bytes,
        reasoning_bytes: summary.reasoning_bytes,
    };
    summary.emitted_phase_count += 1;
    Some(emission)
}

fn reset_phase_state(summary: &mut ClaudeStdoutSummary, segment: &mut SegmentState) {
    summary.streamed_text.clear();
    summary.streamed_reasoning.clear();
    summary.assistant_text = None;
    summary.result_text = None;
    summary.result_reasoning = None;
    summary.usage = None;
    summary.tool_calls.clear();
    summary.tool_io_bytes = 0;
    summary.reasoning_bytes = 0;
    segment.has_content = false;
}

fn close_current_phase(
    summary: &mut ClaudeStdoutSummary,
    segment: &mut SegmentState,
    inner: &ClaudeInner,
) {
    flush_pending_tool_uses(summary, segment);

    if let Some(phase) = take_phase_emission(summary) {
        let tool_calls = phase
            .tool_calls
            .iter()
            .map(|tool| {
                json!({
                    "id": tool.id,
                    "name": tool.name,
                    "arguments": tool.arguments,
                })
            })
            .collect::<Vec<_>>();
        inner.emit_stream_end(
            phase.text,
            phase.model,
            phase.usage,
            phase.reasoning,
            tool_calls,
            None,
        );
        for tool_call in &phase.tool_calls {
            inner.emit_tool_request(tool_call);
        }
    }

    reset_phase_state(summary, segment);
    segment.awaiting_stream_start = true;
}

fn content_block_index(event: &Value) -> Option<u64> {
    event.get("index").and_then(Value::as_u64)
}

fn extract_tool_json_delta(delta: &Value) -> Option<&str> {
    delta
        .get("partial_json")
        .or_else(|| delta.get("partialJson"))
        .or_else(|| delta.get("json"))
        .or_else(|| delta.get("text"))
        .and_then(Value::as_str)
}

fn register_tool_call_for_phase(
    summary: &mut ClaudeStdoutSummary,
    segment: &mut SegmentState,
    tool_call: ClaudeToolCall,
) {
    if summary.register_tool_call(tool_call.clone()) {
        segment.has_content = true;
    }
}

fn maybe_emit_pending_tool_use(
    summary: &mut ClaudeStdoutSummary,
    segment: &mut SegmentState,
    index: u64,
) {
    let Some(pending) = segment.pending_tool_uses.get_mut(&index) else {
        return;
    };

    if !pending.partial_json.trim().is_empty()
        && let Ok(parsed) = serde_json::from_str::<Value>(&pending.partial_json)
    {
        pending.arguments = parsed;
    }

    if pending.request_emitted {
        return;
    }

    let tool_call = ClaudeToolCall {
        id: pending.id.clone(),
        name: pending.name.clone(),
        arguments: pending.arguments.clone(),
    };
    if !has_meaningful_tool_arguments(&tool_call.arguments) {
        return;
    }

    pending.request_emitted = true;
    register_tool_call_for_phase(summary, segment, tool_call);
}

fn flush_pending_tool_uses(summary: &mut ClaudeStdoutSummary, segment: &mut SegmentState) {
    let indexes = segment
        .pending_tool_uses
        .keys()
        .copied()
        .collect::<Vec<_>>();
    for index in indexes {
        maybe_emit_pending_tool_use(summary, segment, index);
        if let Some(pending) = segment.pending_tool_uses.remove(&index) {
            if pending.request_emitted {
                continue;
            }
            let fallback = ClaudeToolCall {
                id: pending.id,
                name: pending.name,
                arguments: pending.arguments,
            };
            register_tool_call_for_phase(summary, segment, fallback);
        }
    }
}

fn consume_stream_event(
    event: &Value,
    summary: &mut ClaudeStdoutSummary,
    segment: &mut SegmentState,
    inner: &ClaudeInner,
    base_message_id: &str,
    current_message_id: &mut String,
) {
    let event_type = event
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();

    match event_type {
        "message_start" => {
            flush_pending_tool_uses(summary, segment);
            if phase_has_pending_output(summary, segment) {
                close_current_phase(summary, segment, inner);
            }

            let next_model = event
                .get("message")
                .and_then(|message| message.get("model"))
                .and_then(Value::as_str)
                .map(|model| model.to_string());
            let next_message_id = event.get("message").and_then(extract_claude_message_id);

            if let Some(model) = next_model.clone() {
                summary.model = Some(model);
            }
            if let Some(usage) = parse_token_usage(
                event
                    .get("message")
                    .and_then(|message| message.get("usage")),
            ) {
                summary.usage = Some(usage);
            }
            if let Some(message_id) = next_message_id {
                segment.current_claude_message_id = Some(message_id);
            }
            maybe_emit_next_stream_start(
                segment,
                inner,
                base_message_id,
                current_message_id,
                next_model.or_else(|| summary.model.clone()),
            );
        }
        "message_delta" => {
            if let Some(usage) = parse_token_usage(event.get("usage")) {
                summary.usage = Some(usage);
            }
        }
        "content_block_start" => {
            let Some(block) = event.get("content_block") else {
                return;
            };
            let block_type = block
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if block_type == "text" {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
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
            } else if is_reasoning_marker(block_type) {
                if let Some(text) = extract_reasoning_text(block) {
                    maybe_emit_next_stream_start(
                        segment,
                        inner,
                        base_message_id,
                        current_message_id,
                        summary.model.clone(),
                    );
                    append_reasoning_text(summary, &text, false);
                    segment.has_content = true;
                    inner.emit_stream_reasoning_delta(current_message_id, &text);
                }
            } else if block_type == "tool_use"
                && let Some(tool_call) = extract_tool_call_from_block(block)
            {
                maybe_emit_next_stream_start(
                    segment,
                    inner,
                    base_message_id,
                    current_message_id,
                    summary.model.clone(),
                );
                let block_index = content_block_index(event);
                if !has_meaningful_tool_arguments(&tool_call.arguments) {
                    if let Some(index) = block_index {
                        summary
                            .tool_name_by_id
                            .insert(tool_call.id.clone(), tool_call.name.clone());
                        segment.pending_tool_uses.insert(
                            index,
                            PendingClaudeToolUse {
                                id: tool_call.id,
                                name: tool_call.name,
                                arguments: tool_call.arguments,
                                partial_json: String::new(),
                                request_emitted: false,
                            },
                        );
                        segment.has_content = true;
                    } else {
                        register_tool_call_for_phase(summary, segment, tool_call);
                    }
                } else {
                    register_tool_call_for_phase(summary, segment, tool_call);
                }
            }
        }
        "content_block_delta" => {
            let Some(delta) = event.get("delta") else {
                return;
            };
            let delta_type = delta
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default();
            match delta_type {
                "text_delta" => {
                    if let Some(text) = delta.get("text").and_then(Value::as_str) {
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
                _ if is_reasoning_marker(delta_type) => {
                    if let Some(text) = extract_reasoning_text(delta) {
                        maybe_emit_next_stream_start(
                            segment,
                            inner,
                            base_message_id,
                            current_message_id,
                            summary.model.clone(),
                        );
                        append_reasoning_text(summary, &text, false);
                        segment.has_content = true;
                        inner.emit_stream_reasoning_delta(current_message_id, &text);
                    }
                }
                "input_json_delta" => {
                    let Some(index) = content_block_index(event) else {
                        return;
                    };
                    let Some(partial) = extract_tool_json_delta(delta) else {
                        return;
                    };
                    if let Some(pending) = segment.pending_tool_uses.get_mut(&index) {
                        pending.partial_json.push_str(partial);
                    }
                    maybe_emit_pending_tool_use(summary, segment, index);
                }
                _ => {}
            }
        }
        "content_block_stop" => {
            let Some(index) = content_block_index(event) else {
                return;
            };
            maybe_emit_pending_tool_use(summary, segment, index);
            segment.pending_tool_uses.remove(&index);
        }
        "message_stop" => {
            flush_pending_tool_uses(summary, segment);
            if !summary.tool_calls.is_empty() {
                close_current_phase(summary, segment, inner);
            }
        }
        _ if is_reasoning_marker(event_type) => {
            if let Some(text) = extract_reasoning_text(event) {
                maybe_emit_next_stream_start(
                    segment,
                    inner,
                    base_message_id,
                    current_message_id,
                    summary.model.clone(),
                );
                append_reasoning_text(summary, &text, false);
                segment.has_content = true;
                inner.emit_stream_reasoning_delta(current_message_id, &text);
            }
        }
        _ => {}
    }
}

fn append_reasoning_text(
    summary: &mut ClaudeStdoutSummary,
    text: &str,
    separate_with_newline: bool,
) {
    if !contains_non_whitespace(text) {
        return;
    }
    if separate_with_newline && !summary.streamed_reasoning.is_empty() {
        summary.streamed_reasoning.push('\n');
    }
    summary.reasoning_bytes = summary.reasoning_bytes.saturating_add(text.len() as u64);
    summary.streamed_reasoning.push_str(text);
}

fn extract_text_from_message(message: &Value) -> Option<String> {
    let content = message.get("content")?;

    if let Some(text) = content.as_str() {
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
        return None;
    }

    let blocks = content.as_array()?;
    let mut out = String::new();
    for block in blocks {
        let block_type = block
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let maybe_text = if block_type == "text" || block_type.is_empty() {
            block.get("text").and_then(Value::as_str)
        } else {
            None
        };
        if let Some(text) = maybe_text {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(text);
        }
    }

    let trimmed = out.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn extract_reasoning_from_message(message: &Value) -> Option<String> {
    let blocks = message.get("content")?.as_array()?;
    let mut out = String::new();
    for block in blocks {
        let block_type = block
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !is_reasoning_marker(block_type) {
            continue;
        }
        if let Some(text) = extract_reasoning_text(block) {
            if !contains_non_whitespace(&text) {
                continue;
            }
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&text);
        }
    }
    if out.is_empty() { None } else { Some(out) }
}

fn extract_reasoning_from_result(value: &Value) -> Option<String> {
    for key in [
        "thinking",
        "reasoning",
        "summary",
        "summaryText",
        "summary_text",
        "reasoningSummary",
        "reasoning_summary",
        "reasoningSummaryText",
        "reasoning_summary_text",
        "thinkingSummary",
        "thinking_summary",
        "thinkingSummaryText",
        "thinking_summary_text",
        "thinkingText",
        "thinking_text",
        "reasoningText",
        "reasoning_text",
    ] {
        if let Some(text) = value.get(key).and_then(extract_reasoning_text)
            && contains_non_whitespace(&text)
        {
            return Some(text);
        }
    }

    if let Some(message) = value.get("message")
        && let Some(reasoning) = extract_reasoning_from_message(message)
        && contains_non_whitespace(&reasoning)
    {
        return Some(reasoning);
    }

    None
}

fn is_reasoning_marker(marker: &str) -> bool {
    matches!(
        marker.trim(),
        "thinking" | "thinking_delta" | "reasoning" | "reasoning_delta" | "reasoning_summary"
    )
}

fn extract_reasoning_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => {
            if !contains_non_whitespace(text) {
                None
            } else {
                Some(text.to_string())
            }
        }
        Value::Array(parts) => {
            let mut out = String::new();
            for part in parts {
                if let Some(text) = extract_reasoning_text(part) {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(&text);
                }
            }
            if contains_non_whitespace(&out) {
                Some(out)
            } else {
                None
            }
        }
        Value::Object(map) => {
            for key in [
                "thinking",
                "reasoning",
                "text",
                "text_delta",
                "textDelta",
                "summary",
                "summaryText",
                "summary_text",
                "thinkingSummary",
                "thinking_summary",
                "thinkingSummaryText",
                "thinking_summary_text",
                "reasoningSummary",
                "reasoning_summary",
                "reasoningSummaryText",
                "reasoning_summary_text",
                "thinkingText",
                "thinking_text",
                "reasoningText",
                "reasoning_text",
                "thinking_delta",
                "thinkingDelta",
                "reasoning_delta",
                "reasoningDelta",
                "output_text",
                "outputText",
                "value",
                "delta",
                "content",
                "parts",
            ] {
                if let Some(text) = map.get(key).and_then(extract_reasoning_text) {
                    return Some(text);
                }
            }
            None
        }
        _ => None,
    }
}

fn contains_non_whitespace(text: &str) -> bool {
    text.chars().any(|ch| !ch.is_whitespace())
}

fn extract_tool_calls_from_message(message: &Value) -> Vec<ClaudeToolCall> {
    let Some(blocks) = message.get("content").and_then(Value::as_array) else {
        return Vec::new();
    };

    let mut calls = Vec::new();
    for block in blocks {
        if let Some(tool_call) = extract_tool_call_from_block(block) {
            calls.push(tool_call);
        }
    }
    calls
}

fn extract_tool_call_from_block(block: &Value) -> Option<ClaudeToolCall> {
    if block.get("type").and_then(Value::as_str) != Some("tool_use") {
        return None;
    }
    let id = block
        .get("id")
        .and_then(Value::as_str)
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())?;
    let name = block
        .get("name")
        .and_then(Value::as_str)
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "tool".to_string());
    let arguments = block.get("input").cloned().unwrap_or(Value::Null);

    Some(ClaudeToolCall {
        id,
        name,
        arguments,
    })
}

fn extract_tool_result_content(block: &Value) -> String {
    let Some(content) = block.get("content") else {
        return String::new();
    };
    if let Some(text) = content.as_str() {
        return text.to_string();
    }
    if let Some(parts) = content.as_array() {
        let mut out = String::new();
        for part in parts {
            if let Some(text) = part.get("text").and_then(Value::as_str) {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(text);
                continue;
            }
            if let Some(text) = part.as_str() {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(text);
            }
        }
        if !out.is_empty() {
            return out;
        }
    }
    serde_json::to_string(content).unwrap_or_default()
}

fn first_line_trimmed(text: &str, max_chars: usize) -> String {
    let line = text.lines().next().unwrap_or("").trim();
    let line_chars = line.chars().count();
    if line_chars <= max_chars {
        line.to_string()
    } else {
        let keep = max_chars.saturating_sub(3);
        let mut out = String::new();
        for ch in line.chars().take(keep) {
            out.push(ch);
        }
        out.push_str("...");
        out
    }
}

fn parse_token_usage(raw: Option<&Value>) -> Option<Value> {
    let usage = raw?.as_object()?;

    let input_tokens = usage
        .get("input_tokens")
        .or_else(|| usage.get("inputTokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output_tokens = usage
        .get("output_tokens")
        .or_else(|| usage.get("completion_tokens"))
        .or_else(|| usage.get("outputTokens"))
        .or_else(|| usage.get("completionTokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let total_tokens = usage
        .get("total_tokens")
        .or_else(|| usage.get("totalTokens"))
        .and_then(Value::as_u64)
        .unwrap_or(input_tokens.saturating_add(output_tokens));
    let context_window = usage
        .get("context_window")
        .or_else(|| usage.get("contextWindow"))
        .or_else(|| usage.get("max_input_tokens"))
        .or_else(|| usage.get("maxInputTokens"))
        .and_then(Value::as_u64);

    if input_tokens == 0 && output_tokens == 0 && total_tokens == 0 {
        return None;
    }

    Some(json!({
        "input_tokens": input_tokens,
        "output_tokens": output_tokens,
        "total_tokens": total_tokens,
        "cached_prompt_tokens": usage
            .get("cache_read_input_tokens")
            .or_else(|| usage.get("cached_prompt_tokens"))
            .or_else(|| usage.get("cacheReadInputTokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
        "cache_creation_input_tokens": usage
            .get("cache_creation_input_tokens")
            .or_else(|| usage.get("cacheCreationInputTokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
        "reasoning_tokens": usage
            .get("reasoning_tokens")
            .or_else(|| usage.get("reasoningTokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
        "context_window": context_window,
    }))
}

fn usage_value_u64(usage: &Value, key: &str) -> u64 {
    usage.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn derive_turn_token_usage(current: &Value, previous: Option<&Value>) -> Option<Value> {
    let input_tokens = usage_value_u64(current, "input_tokens");
    let output_tokens = usage_value_u64(current, "output_tokens");
    let total_tokens = usage_value_u64(current, "total_tokens");
    let cached_prompt_tokens = usage_value_u64(current, "cached_prompt_tokens");
    let cache_creation_input_tokens = usage_value_u64(current, "cache_creation_input_tokens");
    let reasoning_tokens = usage_value_u64(current, "reasoning_tokens");
    let context_window = current.get("context_window").and_then(Value::as_u64);

    if input_tokens == 0
        && output_tokens == 0
        && total_tokens == 0
        && cached_prompt_tokens == 0
        && cache_creation_input_tokens == 0
        && reasoning_tokens == 0
    {
        return None;
    }

    let Some(previous) = previous else {
        return Some(current.clone());
    };

    let prev_input_tokens = usage_value_u64(previous, "input_tokens");
    let prev_output_tokens = usage_value_u64(previous, "output_tokens");
    let prev_total_tokens = usage_value_u64(previous, "total_tokens");
    let prev_cached_prompt_tokens = usage_value_u64(previous, "cached_prompt_tokens");
    let prev_cache_creation_input_tokens = usage_value_u64(previous, "cache_creation_input_tokens");
    let prev_reasoning_tokens = usage_value_u64(previous, "reasoning_tokens");

    // Claude reports cumulative usage for the whole session.
    // If counters reset (new session/resume), use current values as-is.
    if total_tokens < prev_total_tokens {
        return Some(current.clone());
    }

    let turn_input_tokens = input_tokens.saturating_sub(prev_input_tokens);
    let turn_output_tokens = output_tokens.saturating_sub(prev_output_tokens);
    let turn_total_tokens = total_tokens.saturating_sub(prev_total_tokens);
    let turn_cached_prompt_tokens = cached_prompt_tokens.saturating_sub(prev_cached_prompt_tokens);
    let turn_cache_creation_input_tokens =
        cache_creation_input_tokens.saturating_sub(prev_cache_creation_input_tokens);
    let turn_reasoning_tokens = reasoning_tokens.saturating_sub(prev_reasoning_tokens);

    if turn_input_tokens == 0
        && turn_output_tokens == 0
        && turn_total_tokens == 0
        && turn_cached_prompt_tokens == 0
        && turn_cache_creation_input_tokens == 0
        && turn_reasoning_tokens == 0
    {
        return None;
    }

    Some(json!({
        "input_tokens": turn_input_tokens,
        "output_tokens": turn_output_tokens,
        "total_tokens": turn_total_tokens,
        "cached_prompt_tokens": turn_cached_prompt_tokens,
        "cache_creation_input_tokens": turn_cache_creation_input_tokens,
        "reasoning_tokens": turn_reasoning_tokens,
        "context_window": context_window,
    }))
}

fn extract_result_error(value: &Value) -> Option<String> {
    if let Some(error) = value.get("error") {
        if let Some(message) = error.get("message").and_then(Value::as_str) {
            let trimmed = message.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
        if let Some(message) = error.as_str() {
            let trimmed = message.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }

    if let Some(errors) = value.get("errors").and_then(Value::as_array) {
        let mut joined = Vec::new();
        for err in errors {
            if let Some(message) = err.get("message").and_then(Value::as_str) {
                let trimmed = message.trim();
                if !trimmed.is_empty() {
                    joined.push(trimmed.to_string());
                }
            } else if let Some(message) = err.as_str() {
                let trimmed = message.trim();
                if !trimmed.is_empty() {
                    joined.push(trimmed.to_string());
                }
            }
        }
        if !joined.is_empty() {
            return Some(joined.join("; "));
        }
    }

    None
}

fn normalize_nonempty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn normalize_optional_string(value: &Value) -> Option<String> {
    if value.is_null() {
        return None;
    }
    value.as_str().and_then(normalize_nonempty)
}

fn normalize_claude_effort(value: &Value) -> Option<String> {
    let normalized = normalize_optional_string(value)?.to_ascii_lowercase();
    match normalized.as_str() {
        "low" | "medium" | "high" | "max" => Some(normalized),
        "xhigh" | "extra_high" | "extra-high" => Some("max".to_string()),
        "minimal" | "none" => Some("low".to_string()),
        _ => None,
    }
}

fn normalize_claude_permission_mode(value: &Value) -> Option<String> {
    let normalized = normalize_optional_string(value)?.to_ascii_lowercase();
    match normalized.as_str() {
        "acceptedits" => Some("acceptEdits".to_string()),
        "bypasspermissions" => Some("bypassPermissions".to_string()),
        // Tyde currently runs Claude without permission gating; treat legacy/default
        // values as bypass to avoid approval prompts for existing sessions.
        "default" => Some("bypassPermissions".to_string()),
        "delegate" => Some("delegate".to_string()),
        "dontask" => Some("dontAsk".to_string()),
        "plan" => Some("plan".to_string()),
        _ => None,
    }
}

fn estimate_turn_input_bytes(prompt: &str, images: &[ImageAttachment]) -> u64 {
    let mut total = prompt.len() as u64;
    for image in images {
        total = total
            .saturating_add(image.data.len() as u64)
            .saturating_add(image.media_type.len() as u64);
    }
    total
}

fn build_stream_json_user_message(prompt: &str, images: &[ImageAttachment]) -> Value {
    let mut content_blocks = vec![json!({
        "type": "text",
        "text": prompt,
    })];

    for image in images {
        let media_type =
            normalize_nonempty(&image.media_type).unwrap_or_else(|| "image/png".to_string());
        if image.data.trim().is_empty() {
            continue;
        }
        content_blocks.push(json!({
            "type": "image",
            "source": {
                "type": "base64",
                "media_type": media_type,
                "data": image.data,
            }
        }));
    }

    json!({
        "type": "user",
        "message": {
            "role": "user",
            "content": content_blocks,
        }
    })
}

fn normalize_tool_name(tool_name: &str) -> String {
    tool_name
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase()
}

fn claude_is_modify_tool_name(tool_name: &str) -> bool {
    matches!(
        normalize_tool_name(tool_name).as_str(),
        "edit" | "multiedit" | "write" | "notebookedit" | "applypatch"
    )
}

fn claude_is_run_command_tool_name(tool_name: &str) -> bool {
    normalize_tool_name(tool_name) == "bash"
}

fn claude_is_read_tool_name(tool_name: &str) -> bool {
    matches!(
        normalize_tool_name(tool_name).as_str(),
        "read" | "notebookread"
    )
}

fn claude_is_todo_write_tool_name(tool_name: &str) -> bool {
    normalize_tool_name(tool_name) == "todowrite"
}

fn claude_is_user_input_tool_name(tool_name: &str) -> bool {
    matches!(
        normalize_tool_name(tool_name).as_str(),
        "askuserquestion" | "exitplanmode" | "enterplanmode"
    )
}

/// Convert a TodoWrite tool call's arguments into a TaskUpdate event value.
///
/// Claude Code's TodoWrite sends `{ "todos": [{ "content": "...", "status": "...", "activeForm": "..." }, ...] }`.
/// We map this to our protocol's `TaskUpdate` → `TaskList { title, tasks: [Task { id, description, status }] }`.
/// For in-progress tasks the `activeForm` field is used as the description (present-tense),
/// otherwise `content` (imperative form).
fn claude_task_update_from_todo_write(arguments: &Value) -> Option<Value> {
    let todos = arguments.get("todos")?.as_array()?;
    let mut tasks = Vec::with_capacity(todos.len());
    for (i, todo) in todos.iter().enumerate() {
        let status = todo
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("pending");
        let description = if status == "in_progress" {
            todo.get("activeForm")
                .and_then(Value::as_str)
                .or_else(|| todo.get("content").and_then(Value::as_str))
        } else {
            todo.get("content")
                .and_then(Value::as_str)
                .or_else(|| todo.get("activeForm").and_then(Value::as_str))
        }
        .unwrap_or("");
        tasks.push(json!({
            "id": i,
            "description": description,
            "status": status,
        }));
    }
    Some(json!({
        "kind": "TaskUpdate",
        "data": {
            "title": "",
            "tasks": tasks,
        }
    }))
}

#[derive(Debug, Clone)]
struct ClaudeRunCommandResult {
    exit_code: i32,
    stdout: String,
    stderr: String,
}

impl ClaudeRunCommandResult {
    fn as_tool_result(&self) -> Value {
        json!({
            "kind": "RunCommand",
            "exit_code": self.exit_code,
            "stdout": self.stdout,
            "stderr": self.stderr,
        })
    }
}

fn claude_run_command_result_from_tool_block(
    block: &Value,
    result_text: &str,
    default_exit_code: i32,
    treat_text_as_stderr: bool,
) -> ClaudeRunCommandResult {
    let mut parsed = block
        .get("result")
        .and_then(|value| parse_run_command_result_from_value(value, default_exit_code))
        .or_else(|| {
            block
                .get("content")
                .and_then(|value| parse_run_command_result_from_value(value, default_exit_code))
        })
        .or_else(|| {
            serde_json::from_str::<Value>(result_text)
                .ok()
                .and_then(|value| parse_run_command_result_from_value(&value, default_exit_code))
        })
        .unwrap_or(ClaudeRunCommandResult {
            exit_code: default_exit_code,
            stdout: String::new(),
            stderr: String::new(),
        });

    if let Some(code) = parse_exit_code_from_text(result_text) {
        parsed.exit_code = code;
    }

    if parsed.stdout.trim().is_empty() && parsed.stderr.trim().is_empty() {
        if let Some((stdout, stderr)) = parse_command_output_sections(result_text) {
            parsed.stdout = stdout;
            parsed.stderr = stderr;
        } else if treat_text_as_stderr {
            parsed.stderr = result_text.to_string();
        } else {
            parsed.stdout = result_text.to_string();
        }
    }

    parsed
}

fn parse_run_command_result_from_value(
    value: &Value,
    default_exit_code: i32,
) -> Option<ClaudeRunCommandResult> {
    match value {
        Value::Object(map) => {
            let exit_code = [
                "exit_code",
                "exitCode",
                "code",
                "return_code",
                "returnCode",
                "status",
            ]
            .iter()
            .find_map(|key| value_to_i32(map.get(*key)))
            .unwrap_or(default_exit_code);
            let stdout = map
                .get("stdout")
                .or_else(|| map.get("output"))
                .or_else(|| map.get("std_out"))
                .map(value_to_string)
                .unwrap_or_default();
            let stderr = map
                .get("stderr")
                .or_else(|| map.get("error"))
                .or_else(|| map.get("std_err"))
                .map(value_to_string)
                .unwrap_or_default();

            if stdout.is_empty() && stderr.is_empty() && exit_code == default_exit_code {
                return None;
            }

            Some(ClaudeRunCommandResult {
                exit_code,
                stdout,
                stderr,
            })
        }
        Value::String(text) => serde_json::from_str::<Value>(text)
            .ok()
            .and_then(|parsed| parse_run_command_result_from_value(&parsed, default_exit_code)),
        _ => None,
    }
}

fn value_to_i32(value: Option<&Value>) -> Option<i32> {
    let raw = value?;
    if let Some(number) = raw.as_i64() {
        return i32::try_from(number).ok();
    }
    raw.as_str()
        .and_then(|text| text.trim().parse::<i32>().ok())
}

fn value_to_string(value: &Value) -> String {
    if let Some(text) = value.as_str() {
        return text.to_string();
    }
    serde_json::to_string(value).unwrap_or_default()
}

fn parse_command_output_sections(text: &str) -> Option<(String, String)> {
    let mut stdout_lines: Vec<String> = Vec::new();
    let mut stderr_lines: Vec<String> = Vec::new();
    let mut section: Option<&str> = None;
    let mut saw_marker = false;

    for raw_line in text.lines() {
        let trimmed_start = raw_line.trim_start();
        let lower = trimmed_start.to_ascii_lowercase();
        if lower.starts_with("stdout:") {
            saw_marker = true;
            section = Some("stdout");
            let (_, rest) = trimmed_start.split_at("stdout:".len());
            let rest = rest.trim_start();
            if !rest.is_empty() {
                stdout_lines.push(rest.to_string());
            }
            continue;
        }
        if lower.starts_with("stderr:") {
            saw_marker = true;
            section = Some("stderr");
            let (_, rest) = trimmed_start.split_at("stderr:".len());
            let rest = rest.trim_start();
            if !rest.is_empty() {
                stderr_lines.push(rest.to_string());
            }
            continue;
        }

        match section {
            Some("stdout") => stdout_lines.push(raw_line.to_string()),
            Some("stderr") => stderr_lines.push(raw_line.to_string()),
            _ => {}
        }
    }

    if !saw_marker {
        return None;
    }

    Some((
        stdout_lines.join("\n").trim().to_string(),
        stderr_lines.join("\n").trim().to_string(),
    ))
}

fn parse_exit_code_from_text(text: &str) -> Option<i32> {
    for line in text.lines() {
        let lower = line.to_ascii_lowercase();
        if !lower.contains("exit") {
            continue;
        }
        if let Some(value) = extract_first_i32(line) {
            return Some(value);
        }
    }
    None
}

fn extract_first_i32(text: &str) -> Option<i32> {
    let mut token = String::new();
    for ch in text.chars() {
        if ch == '-' && token.is_empty() {
            token.push(ch);
            continue;
        }
        if ch.is_ascii_digit() {
            token.push(ch);
            continue;
        }
        if !token.is_empty()
            && token != "-"
            && let Ok(parsed) = token.parse::<i32>()
        {
            return Some(parsed);
        }
        token.clear();
    }

    if !token.is_empty() && token != "-" {
        return token.parse::<i32>().ok();
    }

    None
}

fn run_command_failure_summary(result: &ClaudeRunCommandResult, fallback: &str) -> String {
    if !result.stderr.trim().is_empty() {
        return first_line_trimmed(&result.stderr, 140);
    }
    if !fallback.trim().is_empty() {
        return first_line_trimmed(fallback, 140);
    }
    format!("Command failed with exit code {}", result.exit_code)
}

fn claude_argument_string(arguments: &Value, keys: &[&str]) -> Option<String> {
    for key in keys {
        if let Some(value) = arguments.get(*key).and_then(Value::as_str)
            && let Some(normalized) = normalize_nonempty(value)
        {
            return Some(normalized);
        }
    }
    None
}

fn claude_argument_file_path(arguments: &Value) -> Option<String> {
    claude_argument_string(
        arguments,
        &[
            "file_path",
            "path",
            "filename",
            "notebook_path",
            "target_file",
        ],
    )
}

fn claude_argument_file_paths(arguments: &Value) -> Vec<String> {
    let mut paths = Vec::new();
    if let Some(path) = claude_argument_file_path(arguments) {
        paths.push(path);
    }

    for key in ["file_paths", "paths"] {
        let Some(values) = arguments.get(key).and_then(Value::as_array) else {
            continue;
        };
        for value in values {
            if let Some(path) = value.as_str().and_then(normalize_nonempty)
                && !paths.iter().any(|existing| existing == &path)
            {
                paths.push(path);
            }
        }
    }

    paths
}

fn estimate_line_delta(before: &str, after: &str) -> (u64, u64) {
    let before_lines = if before.is_empty() {
        Vec::new()
    } else {
        before.lines().collect::<Vec<_>>()
    };
    let after_lines = if after.is_empty() {
        Vec::new()
    } else {
        after.lines().collect::<Vec<_>>()
    };

    let mut start = 0usize;
    while start < before_lines.len()
        && start < after_lines.len()
        && before_lines[start] == after_lines[start]
    {
        start += 1;
    }

    let mut end_before = before_lines.len();
    let mut end_after = after_lines.len();
    while end_before > start
        && end_after > start
        && before_lines[end_before - 1] == after_lines[end_after - 1]
    {
        end_before -= 1;
        end_after -= 1;
    }

    (
        (end_after.saturating_sub(start)) as u64,
        (end_before.saturating_sub(start)) as u64,
    )
}

fn parse_edit_pair(arguments: &Value) -> Option<(String, String)> {
    let before = claude_argument_string(arguments, &["old_string", "old_text", "oldText", "old"])
        .unwrap_or_default();
    let after = claude_argument_string(arguments, &["new_string", "new_text", "newText", "new"])
        .unwrap_or_default();
    if before.is_empty() && after.is_empty() {
        None
    } else {
        Some((before, after))
    }
}

fn parse_multiedit_preview(arguments: &Value) -> Option<(String, String)> {
    let Some(edits) = arguments.get("edits").and_then(Value::as_array) else {
        return parse_edit_pair(arguments);
    };

    let mut before_chunks = Vec::new();
    let mut after_chunks = Vec::new();
    for edit in edits {
        let Some((before, after)) = parse_edit_pair(edit) else {
            continue;
        };
        before_chunks.push(before);
        after_chunks.push(after);
    }

    if before_chunks.is_empty() && after_chunks.is_empty() {
        return None;
    }

    Some((before_chunks.join("\n"), after_chunks.join("\n")))
}

fn claude_modify_preview(tool_name: &str, arguments: &Value) -> Option<ClaudeModifyPreview> {
    if !claude_is_modify_tool_name(tool_name) {
        return None;
    }
    let file_path = claude_argument_file_path(arguments)?;
    let normalized_tool = normalize_tool_name(tool_name);

    let (before, after) = match normalized_tool.as_str() {
        "write" => {
            let after = claude_argument_string(arguments, &["content", "text", "new_content"])
                .unwrap_or_default();
            (String::new(), after)
        }
        "multiedit" => parse_multiedit_preview(arguments)?,
        "edit" | "notebookedit" => parse_edit_pair(arguments).or_else(|| {
            claude_argument_string(arguments, &["content", "text", "new_content"])
                .map(|after| (String::new(), after))
        })?,
        "applypatch" => {
            // Without explicit before/after snapshots we cannot render a reliable diff preview.
            return None;
        }
        _ => return None,
    };

    let (lines_added, lines_removed) = estimate_line_delta(&before, &after);
    Some(ClaudeModifyPreview {
        file_path,
        before,
        after,
        lines_added,
        lines_removed,
    })
}

fn claude_tool_request_type(tool_name: &str, arguments: &Value) -> Value {
    if let Some(preview) = claude_modify_preview(tool_name, arguments) {
        return json!({
            "kind": "ModifyFile",
            "file_path": preview.file_path,
            "before": preview.before,
            "after": preview.after,
        });
    }

    if claude_is_run_command_tool_name(tool_name) {
        let command = arguments
            .get("command")
            .and_then(Value::as_str)
            .or_else(|| arguments.get("cmd").and_then(Value::as_str))
            .unwrap_or("")
            .to_string();
        let working_directory = arguments
            .get("cwd")
            .or_else(|| arguments.get("working_directory"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        return json!({
            "kind": "RunCommand",
            "command": command,
            "working_directory": working_directory,
        });
    }

    if claude_is_read_tool_name(tool_name) {
        return json!({
            "kind": "ReadFiles",
            "file_paths": claude_argument_file_paths(arguments),
        });
    }

    json!({
        "kind": "Other",
        "args": {
            "tool": tool_name,
            "arguments": arguments,
        }
    })
}

fn claude_home_dir() -> Result<PathBuf, String> {
    if let Ok(explicit) = std::env::var("CLAUDE_CONFIG_DIR") {
        let trimmed = explicit.trim();
        if !trimmed.is_empty() {
            return Ok(PathBuf::from(trimmed));
        }
    }

    if let Ok(home) = std::env::var("HOME") {
        let trimmed = home.trim();
        if !trimmed.is_empty() {
            return Ok(PathBuf::from(trimmed).join(".claude"));
        }
    }

    if let Ok(home) = std::env::var("USERPROFILE") {
        let trimmed = home.trim();
        if !trimmed.is_empty() {
            return Ok(PathBuf::from(trimmed).join(".claude"));
        }
    }

    Err("Unable to resolve Claude home directory".to_string())
}

fn encode_workspace_root(workspace_root: &str) -> String {
    let trimmed = workspace_root.trim();
    if trimmed.is_empty() {
        return "-".to_string();
    }
    trimmed
        .chars()
        .map(|ch| {
            if ch == '/' || ch == '\\' || ch == ':' || ch == '.' {
                '-'
            } else {
                ch
            }
        })
        .collect::<String>()
}

fn normalize_claude_workspace_root(workspace_root: &str) -> String {
    let path = Path::new(workspace_root);
    std::fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

fn claude_workspace_sessions_dir(workspace_root: &str) -> Result<PathBuf, String> {
    let claude_home = claude_home_dir()?;
    Ok(claude_home
        .join("projects")
        .join(encode_workspace_root(&normalize_claude_workspace_root(
            workspace_root,
        ))))
}

fn claude_session_file_path(workspace_root: &str, session_id: &str) -> Result<PathBuf, String> {
    let id = normalize_nonempty(session_id).ok_or("Invalid session id")?;
    Ok(claude_workspace_sessions_dir(workspace_root)?.join(format!("{id}.jsonl")))
}

async fn list_claude_sessions(workspace_root: &str) -> Result<Vec<Value>, String> {
    let sessions_dir = claude_workspace_sessions_dir(workspace_root)?;
    let mut rd = match tokio_fs::read_dir(&sessions_dir).await {
        Ok(rd) => rd,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => {
            return Err(format!(
                "Failed to read Claude sessions directory '{}': {err}",
                sessions_dir.display()
            ));
        }
    };

    let mut sessions = Vec::new();
    while let Ok(Some(entry)) = rd.next_entry().await {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
            continue;
        }
        if let Some(file_name) = path.file_name().and_then(|n| n.to_str())
            && file_name.starts_with("agent-")
        {
            continue;
        }
        if let Some(metadata) = inspect_claude_session_file(&path, workspace_root).await? {
            sessions.push(metadata);
        }
    }

    sessions.sort_by(|a, b| {
        let a_ts = a.get("last_modified").and_then(Value::as_u64).unwrap_or(0);
        let b_ts = b.get("last_modified").and_then(Value::as_u64).unwrap_or(0);
        b_ts.cmp(&a_ts)
    });

    Ok(sessions)
}

async fn inspect_claude_session_file(
    path: &Path,
    workspace_root: &str,
) -> Result<Option<Value>, String> {
    let metadata = tokio_fs::metadata(path).await.map_err(|err| {
        format!(
            "Failed to inspect Claude session '{}': {err}",
            path.display()
        )
    })?;
    if !metadata.is_file() {
        return Ok(None);
    }

    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    let created_at = metadata
        .created()
        .or_else(|_| metadata.modified())
        .ok()
        .map(system_time_to_ms)
        .unwrap_or_else(unix_now_ms);
    let last_modified = metadata
        .modified()
        .ok()
        .map(system_time_to_ms)
        .unwrap_or(created_at);

    let contents = tokio_fs::read_to_string(path)
        .await
        .map_err(|err| format!("Failed to read Claude session '{}': {err}", path.display()))?;

    Ok(inspect_claude_session_contents(
        file_name,
        &contents,
        workspace_root,
        created_at,
        last_modified,
    ))
}

/// Pure parsing of Claude session file contents — shared by local and remote
/// code paths.
fn inspect_claude_session_contents(
    file_name: &str,
    contents: &str,
    workspace_root: &str,
    created_at: u64,
    last_modified: u64,
) -> Option<Value> {
    let mut session_id = file_name
        .strip_suffix(".jsonl")
        .unwrap_or(file_name)
        .to_string();

    let mut preview = String::new();
    let mut message_count = 0u64;
    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let value = match serde_json::from_str::<Value>(trimmed) {
            Ok(value) => value,
            Err(_) => continue,
        };

        if let Some(raw_session_id) = value
            .get("sessionId")
            .or_else(|| value.get("session_id"))
            .and_then(Value::as_str)
            .and_then(normalize_nonempty)
        {
            session_id = raw_session_id;
        }

        let line_type = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if line_type == "assistant" || line_type == "user" {
            message_count = message_count.saturating_add(1);
            if let Some(candidate) = extract_preview_from_session_line(&value) {
                preview = candidate;
            }
        }
    }

    let title = if preview.trim().is_empty() {
        "Claude Session".to_string()
    } else {
        preview.clone()
    };

    Some(json!({
        "id": session_id,
        "session_id": session_id,
        "title": title,
        "created_at": created_at,
        "last_modified": last_modified,
        "last_message_preview": preview,
        "workspace_root": workspace_root,
        "message_count": message_count,
        "backend_kind": "claude",
    }))
}

fn extract_preview_from_session_line(value: &Value) -> Option<String> {
    let message = value.get("message")?;
    let content = message.get("content")?;

    if let Some(text) = content.as_str() {
        return normalize_nonempty(text);
    }

    let mut fallback_tool = None::<String>;
    if let Some(blocks) = content.as_array() {
        let mut out = String::new();
        for block in blocks {
            let block_type = block
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if block_type == "text" {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    let trimmed = text.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    if !out.is_empty() {
                        out.push(' ');
                    }
                    out.push_str(trimmed);
                }
            } else if block_type == "tool_use" && fallback_tool.is_none() {
                if let Some(name) = block.get("name").and_then(Value::as_str) {
                    fallback_tool = Some(format!("Used tool {name}"));
                }
            } else if block_type == "tool_result" && fallback_tool.is_none() {
                fallback_tool = Some("Tool result".to_string());
            }
        }
        if let Some(text) = normalize_nonempty(&out) {
            return Some(text);
        }
    }

    if let Some(result) = value.get("toolUseResult").and_then(Value::as_str) {
        return normalize_nonempty(result);
    }
    fallback_tool
}

async fn load_claude_session_history(
    workspace_root: &str,
    session_id: &str,
) -> Result<ClaudeSessionReplay, String> {
    let session_file = claude_session_file_path(workspace_root, session_id)?;
    let mut last_err = None;
    for attempt in 0..20 {
        match tokio_fs::read_to_string(&session_file).await {
            Ok(contents) => return Ok(parse_claude_session_replay(&contents)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound && attempt < 19 => {
                last_err = Some(err);
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            Err(err) => {
                return Err(format!(
                    "Failed to read Claude session '{}' for resume: {err}",
                    session_file.display()
                ));
            }
        }
    }

    let err = last_err.unwrap_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "Claude session file did not appear in time",
        )
    });
    Err(format!(
        "Failed to read Claude session '{}' for resume: {err}",
        session_file.display()
    ))
}

#[cfg(test)]
fn parse_claude_session_history_contents(contents: &str) -> Vec<ClaudeHistoryReplayItem> {
    parse_claude_session_replay(contents).items
}

fn parse_claude_session_replay(contents: &str) -> ClaudeSessionReplay {
    let mut restored = Vec::new();
    let mut last_cumulative_usage = None;
    let mut conversation_bytes_total = 0u64;
    let mut tool_name_by_id = HashMap::<String, String>::new();
    let mut tool_call_by_id = HashMap::<String, ClaudeToolCall>::new();
    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let value = match serde_json::from_str::<Value>(trimmed) {
            Ok(value) => value,
            Err(_) => continue,
        };

        let line_type = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if line_type == "result" {
            if let Some(usage) = parse_token_usage(value.get("usage")) {
                last_cumulative_usage = Some(usage);
            }
            continue;
        }
        if line_type != "assistant" && line_type != "user" {
            continue;
        }

        let Some(message) = value.get("message").and_then(Value::as_object) else {
            continue;
        };
        let Some(role) = message.get("role").and_then(Value::as_str) else {
            continue;
        };
        if role != "assistant" && role != "user" {
            continue;
        }

        let message_value = Value::Object(message.clone());
        let content_value = message.get("content").cloned().unwrap_or(Value::Null);
        let text = extract_text_from_message(&message_value).unwrap_or_default();
        let images = extract_images_from_content(&content_value);
        let reasoning_text = extract_reasoning_from_message(&message_value);
        let reasoning = reasoning_text
            .clone()
            .map(|text| json!({ "text": text }))
            .unwrap_or(Value::Null);
        let token_usage = parse_token_usage(message.get("usage"));
        let tool_calls = if role == "assistant" {
            extract_tool_calls_from_message(&message_value)
        } else {
            Vec::new()
        };
        for tool_call in &tool_calls {
            tool_name_by_id.insert(tool_call.id.clone(), tool_call.name.clone());
            tool_call_by_id.insert(tool_call.id.clone(), tool_call.clone());
        }
        let message_tool_calls: Vec<Value> = tool_calls
            .iter()
            .map(|tool_call| {
                json!({
                    "id": tool_call.id,
                    "name": tool_call.name,
                    "arguments": tool_call.arguments,
                })
            })
            .collect();

        let should_emit_message = if role == "assistant" {
            !text.trim().is_empty()
                || !images.is_empty()
                || !message_tool_calls.is_empty()
                || reasoning_text
                    .as_ref()
                    .is_some_and(|value| !value.trim().is_empty())
        } else {
            !text.trim().is_empty() || !images.is_empty()
        };

        if should_emit_message {
            conversation_bytes_total = conversation_bytes_total
                .saturating_add(estimate_message_history_bytes(&text, &images));
            let sender = if role == "assistant" {
                json!({ "Assistant": { "agent": CLAUDE_AGENT_NAME } })
            } else {
                Value::String("User".to_string())
            };

            let model_info = message
                .get("model")
                .and_then(Value::as_str)
                .and_then(normalize_nonempty)
                .map(|m| json!({ "model": m }))
                .unwrap_or(Value::Null);

            restored.push(ClaudeHistoryReplayItem::Message(json!({
                "timestamp": unix_now_ms(),
                "sender": sender,
                "content": text,
                "reasoning": reasoning,
                "tool_calls": message_tool_calls,
                "model_info": model_info,
                "token_usage": token_usage,
                "context_breakdown": Value::Null,
                "images": images,
            })));
        }

        if role == "assistant" {
            for tool_call in tool_calls {
                restored.push(ClaudeHistoryReplayItem::ToolRequest(tool_call));
            }
        }

        for completion in extract_tool_result_events_from_message(
            &message_value,
            &tool_name_by_id,
            &tool_call_by_id,
        ) {
            restored.push(ClaudeHistoryReplayItem::ToolExecutionCompleted(completion));
        }
    }

    ClaudeSessionReplay {
        items: restored,
        last_cumulative_usage,
        conversation_bytes_total,
    }
}

fn extract_tool_result_events_from_message(
    message: &Value,
    tool_name_by_id: &HashMap<String, String>,
    tool_call_by_id: &HashMap<String, ClaudeToolCall>,
) -> Vec<ClaudeReplayToolExecution> {
    let Some(content) = message.get("content").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut events = Vec::new();

    for block in content {
        if block.get("type").and_then(Value::as_str) != Some("tool_result") {
            continue;
        }

        let Some(tool_use_id) = block.get("tool_use_id").and_then(Value::as_str) else {
            continue;
        };
        let Some(tool_call_id) = normalize_nonempty(tool_use_id) else {
            continue;
        };

        let tool_name = tool_name_by_id
            .get(&tool_call_id)
            .cloned()
            .unwrap_or_else(|| "tool".to_string());
        let modify_preview = tool_call_by_id
            .get(&tool_call_id)
            .and_then(|tool_call| claude_modify_preview(&tool_call.name, &tool_call.arguments));
        let result_text = extract_tool_result_content(block);
        let is_run_command = claude_is_run_command_tool_name(&tool_name);
        let is_read_tool = claude_is_read_tool_name(&tool_name);
        let is_error = block
            .get("is_error")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        if is_error {
            // AskUserQuestion, ExitPlanMode, and EnterPlanMode return is_error in
            // --print mode because they need interactive input. This is expected —
            // treat them as successful end-of-turn signals.
            if claude_is_user_input_tool_name(&tool_name) {
                let tool_result = if normalize_tool_name(&tool_name) == "exitplanmode" {
                    // Find plan file content from a preceding Write tool in this turn.
                    let plan_content = tool_call_by_id
                        .values()
                        .filter(|tc| normalize_tool_name(&tc.name) == "write")
                        .find_map(|tc| {
                            let path = claude_argument_file_path(&tc.arguments)?;
                            if !path.contains(".claude/plans/") {
                                return None;
                            }
                            claude_argument_string(
                                &tc.arguments,
                                &["content", "text", "new_content"],
                            )
                        });
                    match plan_content {
                        Some(content) => json!({
                            "kind": "Other",
                            "result": { "plan_content": content }
                        }),
                        None => json!({ "kind": "Other", "result": null }),
                    }
                } else {
                    json!({ "kind": "Other", "result": null })
                };
                events.push(ClaudeReplayToolExecution {
                    tool_call_id,
                    tool_name,
                    success: true,
                    tool_result,
                    error: None,
                });
                continue;
            }

            if is_run_command {
                let command_result =
                    claude_run_command_result_from_tool_block(block, &result_text, 1, true);
                let summary = run_command_failure_summary(&command_result, &result_text);
                events.push(ClaudeReplayToolExecution {
                    tool_call_id,
                    tool_name,
                    success: false,
                    tool_result: command_result.as_tool_result(),
                    error: Some(summary),
                });
            } else {
                let short = if result_text.trim().is_empty() {
                    "Tool execution failed".to_string()
                } else {
                    first_line_trimmed(&result_text, 140)
                };
                let detail = if result_text.trim().is_empty() {
                    short.clone()
                } else {
                    result_text
                };

                events.push(ClaudeReplayToolExecution {
                    tool_call_id,
                    tool_name,
                    success: false,
                    tool_result: json!({
                        "kind": "Error",
                        "short_message": short,
                        "detailed_message": detail.clone(),
                    }),
                    error: Some(detail),
                });
            }
            continue;
        }

        if let Some(preview) = modify_preview {
            events.push(ClaudeReplayToolExecution {
                tool_call_id,
                tool_name,
                success: true,
                tool_result: json!({
                    "kind": "ModifyFile",
                    "lines_added": preview.lines_added,
                    "lines_removed": preview.lines_removed,
                }),
                error: None,
            });
            continue;
        }

        if claude_is_modify_tool_name(&tool_name) {
            events.push(ClaudeReplayToolExecution {
                tool_call_id,
                tool_name,
                success: true,
                tool_result: json!({
                    "kind": "ModifyFile",
                    "lines_added": 0,
                    "lines_removed": 0,
                }),
                error: None,
            });
            continue;
        }

        if is_run_command {
            let command_result =
                claude_run_command_result_from_tool_block(block, &result_text, 0, false);
            events.push(ClaudeReplayToolExecution {
                tool_call_id,
                tool_name,
                success: true,
                tool_result: command_result.as_tool_result(),
                error: None,
            });
            continue;
        }

        if is_read_tool {
            let files = tool_call_by_id
                .get(&tool_call_id)
                .map(|tool_call| claude_argument_file_paths(&tool_call.arguments))
                .unwrap_or_default()
                .into_iter()
                .map(|path| {
                    json!({
                        "path": path,
                        "bytes": result_text.len()
                    })
                })
                .collect::<Vec<_>>();
            events.push(ClaudeReplayToolExecution {
                tool_call_id,
                tool_name,
                success: true,
                tool_result: json!({
                    "kind": "ReadFiles",
                    "files": files,
                }),
                error: None,
            });
            continue;
        }

        let result = if result_text.trim().is_empty() {
            block.clone()
        } else {
            Value::String(result_text)
        };
        events.push(ClaudeReplayToolExecution {
            tool_call_id,
            tool_name,
            success: true,
            tool_result: json!({
                "kind": "Other",
                "result": result,
            }),
            error: None,
        });
    }

    events
}

fn extract_images_from_content(content: &Value) -> Vec<Value> {
    let Some(blocks) = content.as_array() else {
        return Vec::new();
    };
    let mut images = Vec::new();
    for block in blocks {
        if block.get("type").and_then(Value::as_str) != Some("image") {
            continue;
        }
        let source = block.get("source").unwrap_or(block);
        let media_type = source
            .get("media_type")
            .and_then(Value::as_str)
            .and_then(normalize_nonempty)
            .unwrap_or_else(|| "image/png".to_string());
        let data = source
            .get("data")
            .and_then(Value::as_str)
            .and_then(normalize_nonempty)
            .unwrap_or_default();
        if data.is_empty() {
            continue;
        }
        images.push(json!({
            "media_type": media_type,
            "data": data,
        }));
    }
    images
}

fn estimate_message_history_bytes(text: &str, images: &[Value]) -> u64 {
    let mut total = text.len() as u64;
    for image in images {
        total = total
            .saturating_add(
                image
                    .get("media_type")
                    .and_then(Value::as_str)
                    .map(|value| value.len() as u64)
                    .unwrap_or(0),
            )
            .saturating_add(
                image
                    .get("data")
                    .and_then(Value::as_str)
                    .map(|value| value.len() as u64)
                    .unwrap_or(0),
            );
    }
    total
}

fn claude_known_models() -> Vec<Value> {
    let models = [
        ("claude-opus-4-6", "Claude Opus 4.6", true),
        ("claude-sonnet-4-6", "Claude Sonnet 4.6", false),
        ("claude-haiku-4-5-20251001", "Claude Haiku 4.5", false),
        ("opus", "Opus (latest)", false),
        ("sonnet", "Sonnet (latest)", false),
        ("haiku", "Haiku (latest)", false),
    ];

    models
        .iter()
        .map(|(id, display_name, is_default)| {
            json!({
                "id": id,
                "displayName": display_name,
                "isDefault": is_default,
            })
        })
        .collect()
}

fn normalize_model_key_for_context_lookup(model: &str) -> String {
    strip_context_window_suffix(model.trim()).to_ascii_lowercase()
}

fn strip_context_window_suffix(model: &str) -> &str {
    model.strip_suffix("[1m]").unwrap_or(model)
}

fn claude_model_family_hint(model: &str) -> Option<&'static str> {
    let normalized = normalize_model_key_for_context_lookup(model);
    if normalized.contains("opus") {
        return Some("opus");
    }
    if normalized.contains("sonnet") {
        return Some("sonnet");
    }
    if normalized.contains("haiku") {
        return Some("haiku");
    }
    None
}

fn extract_context_window_from_model_usage_entry(entry: &Value) -> Option<u64> {
    entry
        .get("contextWindow")
        .or_else(|| entry.get("context_window"))
        .and_then(Value::as_u64)
        .filter(|window| *window > 0)
}

fn extract_context_window_from_model_usage(
    model_usage: &serde_json::Map<String, Value>,
    preferred_model: Option<&str>,
) -> Option<u64> {
    let with_window = model_usage
        .iter()
        .filter_map(|(model, entry)| {
            extract_context_window_from_model_usage_entry(entry).map(|window| (model, window))
        })
        .collect::<Vec<_>>();

    if with_window.is_empty() {
        return None;
    }

    if let Some(model) = preferred_model {
        let preferred = normalize_model_key_for_context_lookup(model);
        if let Some((_, window)) = with_window
            .iter()
            .copied()
            .find(|(model_key, _)| normalize_model_key_for_context_lookup(model_key) == preferred)
        {
            return Some(window);
        }

        if let Some(family) = claude_model_family_hint(model)
            && let Some((_, window)) = with_window.iter().copied().find(|(model_key, _)| {
                normalize_model_key_for_context_lookup(model_key).contains(family)
            })
        {
            return Some(window);
        }
    }

    if with_window.len() == 1 {
        return Some(with_window[0].1);
    }

    with_window.first().map(|(_, window)| *window)
}

fn claude_estimated_context_window_for_model(model_hint: Option<&str>) -> u64 {
    let Some(model) = model_hint else {
        return CLAUDE_ESTIMATED_CONTEXT_WINDOW_DEFAULT;
    };
    let normalized = model.trim().to_ascii_lowercase();
    if normalized.ends_with("[1m]") {
        return CLAUDE_ESTIMATED_CONTEXT_WINDOW_1M;
    }
    if normalized.contains("haiku") {
        return CLAUDE_ESTIMATED_CONTEXT_WINDOW_DEFAULT;
    }
    CLAUDE_ESTIMATED_CONTEXT_WINDOW_DEFAULT
}

fn estimate_context_breakdown(
    token_usage: Option<&Value>,
    conversation_history_bytes: u64,
    tool_io_bytes: u64,
    reasoning_bytes: u64,
    known_context_window: Option<u64>,
    model_hint: Option<&str>,
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
    // Use the known context window (from modelUsage), then fall back to the
    // usage object, then fall back to the hardcoded estimate.
    let context_window = known_context_window
        .filter(|w| *w > 0)
        .or_else(|| {
            token_usage
                .and_then(|usage| usage.get("context_window").and_then(Value::as_u64))
                .filter(|window| *window > 0)
        })
        .unwrap_or_else(|| claude_estimated_context_window_for_model(model_hint));
    let reasoning_from_tokens = token_usage
        .and_then(|usage| usage.get("reasoning_tokens").and_then(Value::as_u64))
        .unwrap_or(0)
        .saturating_mul(CLAUDE_ESTIMATED_BYTES_PER_TOKEN);

    let reasoning_est = std::cmp::max(reasoning_bytes, reasoning_from_tokens);
    let observed_bytes = conversation_history_bytes
        .saturating_add(tool_io_bytes)
        .saturating_add(reasoning_est);
    let mut total_prompt_bytes = input_tokens.saturating_mul(CLAUDE_ESTIMATED_BYTES_PER_TOKEN);
    if total_prompt_bytes == 0 {
        total_prompt_bytes = observed_bytes.saturating_add(CLAUDE_MIN_SYSTEM_PROMPT_BYTES);
        input_tokens = total_prompt_bytes.div_ceil(CLAUDE_ESTIMATED_BYTES_PER_TOKEN);
    }

    let mut system_prompt_bytes = std::cmp::min(
        std::cmp::max(CLAUDE_MIN_SYSTEM_PROMPT_BYTES, total_prompt_bytes / 10),
        total_prompt_bytes,
    );
    if total_prompt_bytes == 0 {
        system_prompt_bytes = 0;
    }

    let mut remaining = total_prompt_bytes.saturating_sub(system_prompt_bytes);
    let reasoning_bucket = std::cmp::min(reasoning_est, remaining);
    remaining = remaining.saturating_sub(reasoning_bucket);

    let tool_bucket = std::cmp::min(tool_io_bytes, remaining);
    remaining = remaining.saturating_sub(tool_bucket);

    let history_bucket = std::cmp::min(conversation_history_bytes, remaining);
    remaining = remaining.saturating_sub(history_bucket);

    json!({
        "system_prompt_bytes": system_prompt_bytes,
        "tool_io_bytes": tool_bucket,
        "conversation_history_bytes": history_bucket,
        "reasoning_bytes": reasoning_bucket,
        "context_injection_bytes": remaining,
        "input_tokens": input_tokens,
        "context_window": context_window,
    })
}

fn system_time_to_ms(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_else(|_| unix_now_ms())
}

fn pick_workspace_root(workspace_roots: &[String]) -> Result<String, String> {
    workspace_roots
        .iter()
        .find(|root| !root.trim().is_empty() && !root.starts_with("ssh://"))
        .cloned()
        .ok_or("Claude backend requires at least one local workspace root".to_string())
}

// ---------------------------------------------------------------------------
// Remote (SSH) session file helpers
// ---------------------------------------------------------------------------

async fn list_claude_sessions_remote(
    host: &str,
    workspace_root: &str,
) -> Result<Vec<Value>, String> {
    use crate::remote::run_ssh_raw;

    let encoded = encode_workspace_root(workspace_root);
    tracing::info!(
        "list_claude_sessions_remote: host={host}, workspace_root={workspace_root}, encoded={encoded}"
    );
    // Avoid transferring entire session files (can be megabytes) — instead
    // extract metadata from head+tail in a single SSH round-trip.
    let marker = "___TYDE_SESSION_BOUNDARY___";
    let script = format!(
        "dir=\"$HOME/.claude/projects/{encoded}\"; \
         [ -d \"$dir\" ] || exit 0; \
         for f in \"$dir\"/*.jsonl; do \
           [ -f \"$f\" ] || continue; \
           name=$(basename \"$f\"); \
           cnt=$(grep -c '\"type\":\"' \"$f\" 2>/dev/null || echo 0); \
           echo \"{marker}$name $cnt\"; \
           head -5 \"$f\"; \
           echo; \
           tail -5 \"$f\"; \
         done"
    );
    let output = run_ssh_raw(host, &script).await?;
    let raw = String::from_utf8_lossy(&output.stdout);

    let mut sessions = Vec::new();
    for chunk in raw.split(marker) {
        let chunk = chunk.trim();
        if chunk.is_empty() {
            continue;
        }
        let (header, contents) = match chunk.split_once('\n') {
            Some((h, rest)) => (h.trim(), rest),
            None => continue,
        };
        let (name, msg_count) = match header.rsplit_once(' ') {
            Some((n, c)) => (n.trim(), c.trim().parse::<u64>().unwrap_or(0)),
            None => (header, 0),
        };
        if !name.ends_with(".jsonl") || name.starts_with("agent-") {
            continue;
        }

        let now = unix_now_ms();
        if let Some(mut metadata) =
            inspect_claude_session_contents(name, contents, workspace_root, now, now)
        {
            metadata["message_count"] = serde_json::json!(msg_count);
            sessions.push(metadata);
        }
    }

    sessions.sort_by(|a, b| {
        let a_ts = a.get("last_modified").and_then(Value::as_u64).unwrap_or(0);
        let b_ts = b.get("last_modified").and_then(Value::as_u64).unwrap_or(0);
        b_ts.cmp(&a_ts)
    });

    Ok(sessions)
}

async fn load_claude_session_history_remote(
    host: &str,
    workspace_root: &str,
    session_id: &str,
) -> Result<ClaudeSessionReplay, String> {
    use crate::remote::run_ssh_raw;

    let encoded = encode_workspace_root(workspace_root);
    let id = normalize_nonempty(session_id).ok_or("Invalid session id")?;
    let cmd = format!("cat \"$HOME/.claude/projects/{encoded}/{id}.jsonl\"");
    let output = run_ssh_raw(host, &cmd).await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "Failed to read remote Claude session '{id}': {stderr}"
        ));
    }
    let contents = String::from_utf8_lossy(&output.stdout);
    Ok(parse_claude_session_replay(&contents))
}

async fn delete_claude_session_remote(
    host: &str,
    workspace_root: &str,
    session_id: &str,
) -> Result<(), String> {
    use crate::remote::run_ssh_raw;

    let encoded = encode_workspace_root(workspace_root);
    let cmd = format!("rm -f \"$HOME/.claude/projects/{encoded}/{session_id}.jsonl\"");
    let output = run_ssh_raw(host, &cmd).await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "Failed to delete remote Claude session '{session_id}': {stderr}"
        ));
    }
    Ok(())
}

pub(crate) fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Backend trait implementation
// ---------------------------------------------------------------------------

use protocol::{
    AgentInput, BackendKind, ChatEvent, ChatMessage, MessageSender, SelectOption, SessionId,
    SessionSettingField, SessionSettingFieldType, SessionSettingValue, SessionSettingsSchema,
    SpawnCostHint,
};

use super::{
    Backend, BackendSession, BackendSpawnConfig, EventStream, protocol_images_to_attachments,
    resolve_settings as resolve_backend_settings, session_settings_to_json,
};

const BACKEND_EVENT_BUFFER: usize = 256;
const BACKEND_INPUT_BUFFER: usize = 64;

type ClaudeReadyTx = Arc<Mutex<Option<oneshot::Sender<Result<(), String>>>>>;

/// Minimal Backend-trait handle for the Claude CLI.
///
/// Holds an `mpsc::Sender<AgentInput>` that the spawned task reads from;
/// the task writes stdin of the child process accordingly.
pub struct ClaudeBackend {
    input_tx: mpsc::Sender<AgentInput>,
    interrupt_tx: mpsc::Sender<()>,
    session_id: Arc<std::sync::Mutex<Option<SessionId>>>,
    subagent_emitter_tx: watch::Sender<Option<Arc<dyn SubAgentEmitter>>>,
}

impl ClaudeBackend {
    pub(crate) async fn set_subagent_emitter(&self, emitter: Arc<dyn SubAgentEmitter>) {
        let _ = self.subagent_emitter_tx.send(Some(emitter));
    }
}

fn claude_backend_defaults(
    cost_hint: Option<SpawnCostHint>,
) -> (Option<&'static str>, Option<&'static str>) {
    match cost_hint {
        Some(SpawnCostHint::Low) => (Some("haiku"), Some("low")),
        Some(SpawnCostHint::Medium) => (Some("sonnet"), Some("medium")),
        Some(SpawnCostHint::High) => (Some("opus"), Some("high")),
        None => (None, None),
    }
}

fn claude_cost_hint_defaults(cost_hint: SpawnCostHint) -> protocol::SessionSettingsValues {
    let (model, effort) = claude_backend_defaults(Some(cost_hint));
    let mut values = protocol::SessionSettingsValues::default();
    if let Some(model) = model {
        values.0.insert(
            "model".to_string(),
            SessionSettingValue::String(model.to_string()),
        );
    }
    if let Some(effort) = effort {
        values.0.insert(
            "effort".to_string(),
            SessionSettingValue::String(effort.to_string()),
        );
    }
    values
}

pub(crate) fn resolve_session_settings(
    config: &BackendSpawnConfig,
) -> protocol::SessionSettingsValues {
    resolve_backend_settings(
        config,
        &ClaudeBackend::session_settings_schema(),
        claude_cost_hint_defaults,
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

fn claude_agent_identity(config: &BackendSpawnConfig) -> Option<AgentIdentity> {
    let instructions = config
        .resolved_spawn_config
        .instructions
        .as_ref()
        .map(|text| text.trim())
        .filter(|text| !text.is_empty())?;
    let id = config
        .custom_agent_id
        .as_ref()
        .map(|id| id.0.clone())
        .unwrap_or_else(|| "tyde-custom-agent".to_string());
    Some(AgentIdentity {
        id,
        description: "Tyde custom agent".to_string(),
        instructions: instructions.to_string(),
    })
}

fn claude_steering_content(config: &BackendSpawnConfig) -> Option<String> {
    let mut sections = Vec::new();
    if !config.resolved_spawn_config.steering_body.trim().is_empty() {
        sections.push(
            config
                .resolved_spawn_config
                .steering_body
                .trim()
                .to_string(),
        );
    }
    if !config.resolved_spawn_config.skills.is_empty() {
        let skills = config
            .resolved_spawn_config
            .skills
            .iter()
            .map(|skill| format!("Skill: {}\n{}", skill.name, skill.body.trim()))
            .collect::<Vec<_>>()
            .join("\n\n");
        sections.push(skills);
    }
    if sections.is_empty() {
        None
    } else {
        Some(sections.join("\n\n"))
    }
}

fn spawn_claude_subagent_event_bridge(
    mut raw_rx: mpsc::UnboundedReceiver<Value>,
    event_tx: mpsc::UnboundedSender<ChatEvent>,
) {
    tokio::spawn(async move {
        while let Some(raw) = raw_rx.recv().await {
            let event = match serde_json::from_value::<ChatEvent>(raw.clone()) {
                Ok(event) => event,
                Err(_) => match raw.get("kind").and_then(Value::as_str).unwrap_or_default() {
                    "Error" => {
                        let message = raw
                            .get("data")
                            .and_then(Value::as_str)
                            .unwrap_or("Claude backend error")
                            .to_string();
                        backend_error_message(message)
                    }
                    _ => continue,
                },
            };
            if event_tx.send(event).is_err() {
                break;
            }
        }
    });
}

async fn forward_claude_backend_event(
    raw: Value,
    events_tx: &mpsc::Sender<ChatEvent>,
    session_id_sink: &Arc<std::sync::Mutex<Option<SessionId>>>,
    ready_tx: Option<&ClaudeReadyTx>,
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
                *session_id_sink
                    .lock()
                    .expect("claude session_id mutex poisoned") =
                    Some(SessionId(session_id.to_string()));
                if let Some(ready_tx) = ready_tx {
                    signal_ready(ready_tx, Ok(())).await;
                }
            }
        }
        "Error" => {
            let message = raw
                .get("data")
                .and_then(Value::as_str)
                .unwrap_or("Claude backend error")
                .to_string();
            let session_started = session_id_sink
                .lock()
                .expect("claude session_id mutex poisoned")
                .is_some();
            if !session_started && let Some(ready_tx) = ready_tx {
                signal_ready(ready_tx, Err(message.clone())).await;
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

impl Backend for ClaudeBackend {
    fn session_settings_schema() -> SessionSettingsSchema {
        SessionSettingsSchema {
            backend_kind: BackendKind::Claude,
            fields: vec![
                SessionSettingField {
                    key: "model".to_string(),
                    label: "Model".to_string(),
                    description: None,
                    use_slider: false,
                    field_type: SessionSettingFieldType::Select {
                        options: vec![
                            SelectOption {
                                value: "haiku".to_string(),
                                label: "Haiku".to_string(),
                            },
                            SelectOption {
                                value: "sonnet".to_string(),
                                label: "Sonnet".to_string(),
                            },
                            SelectOption {
                                value: "opus".to_string(),
                                label: "Opus".to_string(),
                            },
                        ],
                        default: Some("sonnet".to_string()),
                        nullable: true,
                    },
                },
                SessionSettingField {
                    key: "effort".to_string(),
                    label: "Effort".to_string(),
                    description: None,
                    use_slider: true,
                    field_type: SessionSettingFieldType::Select {
                        options: vec![
                            SelectOption {
                                value: "low".to_string(),
                                label: "Low".to_string(),
                            },
                            SelectOption {
                                value: "medium".to_string(),
                                label: "Medium".to_string(),
                            },
                            SelectOption {
                                value: "high".to_string(),
                                label: "High".to_string(),
                            },
                            SelectOption {
                                value: "max".to_string(),
                                label: "Max".to_string(),
                            },
                        ],
                        default: Some("high".to_string()),
                        nullable: true,
                    },
                },
            ],
        }
    }

    async fn spawn(
        workspace_roots: Vec<String>,
        config: BackendSpawnConfig,
        initial_input: protocol::SendMessagePayload,
    ) -> Result<(Self, EventStream), String> {
        let (input_tx, mut input_rx) = mpsc::channel::<AgentInput>(BACKEND_INPUT_BUFFER);
        let (interrupt_tx, mut interrupt_rx) = mpsc::channel::<()>(BACKEND_INPUT_BUFFER);
        let (events_tx, events_rx) = mpsc::channel::<ChatEvent>(BACKEND_EVENT_BUFFER);
        let session_id = Arc::new(std::sync::Mutex::new(None));
        let session_id_task = Arc::clone(&session_id);
        let (subagent_emitter_tx, mut subagent_emitter_rx) =
            watch::channel::<Option<Arc<dyn SubAgentEmitter>>>(None);
        let (ready_tx, ready_rx) = oneshot::channel::<Result<(), String>>();

        tokio::spawn(async move {
            let roots = if workspace_roots.is_empty() {
                vec!["/tmp".to_string()]
            } else {
                workspace_roots
            };
            let steering_content = claude_steering_content(&config);
            let agent_identity = claude_agent_identity(&config);
            let (session, mut raw_events) = match ClaudeSession::spawn(
                &roots,
                None,
                &config.startup_mcp_servers,
                steering_content.as_deref(),
                agent_identity.as_ref(),
                config.resolved_spawn_config.tool_policy.clone(),
            )
            .await
            {
                Ok(value) => value,
                Err(err) => {
                    tracing::error!("Failed to spawn Claude session: {err}");
                    let _ = ready_tx.send(Err(format!("Failed to spawn Claude session: {err}")));
                    return;
                }
            };

            let handle = session.command_handle();
            let resolved_settings = resolve_session_settings(&config);
            let model_override = match resolved_settings.0.get("model") {
                Some(SessionSettingValue::String(value)) => Some(value.clone()),
                _ => None,
            };
            let effort_override = match resolved_settings.0.get("effort") {
                Some(SessionSettingValue::String(value)) => Some(value.clone()),
                _ => None,
            };
            if model_override.is_some() || effort_override.is_some() {
                let settings = json!({
                    "model": model_override,
                    "effort": effort_override,
                    "permission_mode": CLAUDE_DEFAULT_PERMISSION_MODE,
                });
                if let Err(err) = handle
                    .execute(SessionCommand::UpdateSettings {
                        settings,
                        persist: false,
                    })
                    .await
                {
                    tracing::error!("Failed to configure Claude session: {err}");
                    let _ =
                        ready_tx.send(Err(format!("Failed to configure Claude session: {err}")));
                    session.shutdown().await;
                    return;
                }
            }

            let maybe_emitter = subagent_emitter_rx.borrow().clone();
            if let Some(emitter) = maybe_emitter {
                session.set_subagent_emitter(emitter).await;
            }

            let ready_tx: ClaudeReadyTx = Arc::new(Mutex::new(Some(ready_tx)));
            let ready_tx_forward = Arc::clone(&ready_tx);
            let session_id_forward = Arc::clone(&session_id_task);
            let events_tx_forward = events_tx.clone();
            let forward_task = tokio::spawn(async move {
                while let Some(raw) = raw_events.recv().await {
                    if !forward_claude_backend_event(
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
                signal_ready(
                    &ready_tx_forward,
                    Err("Claude session ended before reporting a session_id".to_string()),
                )
                .await;
            });

            if let Err(err) = handle
                .execute(SessionCommand::SendMessage {
                    message: initial_input.message,
                    images: protocol_images_to_attachments(initial_input.images),
                })
                .await
            {
                tracing::error!("Failed to send initial Claude prompt: {err}");
                signal_ready(
                    &ready_tx,
                    Err(format!("Failed to send initial Claude prompt: {err}")),
                )
                .await;
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
                                        tracing::error!("Failed to send Claude follow-up: {err}");
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
                                        tracing::error!("Failed to update Claude session settings: {err}");
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
                                tracing::error!("Failed to interrupt Claude turn: {err}");
                                break;
                            }
                        }
                        changed = subagent_emitter_rx.changed() => {
                            if changed.is_err() {
                                break;
                            }
                            let maybe_emitter = subagent_emitter_rx.borrow().clone();
                if let Some(emitter) = maybe_emitter {
                                session.set_subagent_emitter(emitter).await;
                            }
                        }
                    }
            }

            session.shutdown().await;
            let _ = forward_task.await;
        });

        match tokio::time::timeout(Duration::from_secs(120), ready_rx).await {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(err))) => return Err(err),
            Ok(Err(_)) => return Err("Claude spawn initialization task ended early".to_string()),
            Err(_) => return Err("Timed out waiting for Claude session_id".to_string()),
        }

        Ok((
            Self {
                input_tx,
                interrupt_tx,
                session_id,
                subagent_emitter_tx,
            },
            EventStream::new(events_rx),
        ))
    }

    async fn resume(
        workspace_roots: Vec<String>,
        config: BackendSpawnConfig,
        session_id: protocol::SessionId,
    ) -> Result<(Self, EventStream), String> {
        let (input_tx, mut input_rx) = mpsc::channel::<AgentInput>(BACKEND_INPUT_BUFFER);
        let (interrupt_tx, mut interrupt_rx) = mpsc::channel::<()>(BACKEND_INPUT_BUFFER);
        let (events_tx, events_rx) = mpsc::channel::<ChatEvent>(BACKEND_EVENT_BUFFER);
        let (subagent_emitter_tx, mut subagent_emitter_rx) =
            watch::channel::<Option<Arc<dyn SubAgentEmitter>>>(None);

        let roots = if workspace_roots.is_empty() {
            vec!["/tmp".to_string()]
        } else {
            workspace_roots
        };
        let session_id = session_id.0;
        let backend_session_id =
            Arc::new(std::sync::Mutex::new(Some(SessionId(session_id.clone()))));
        let backend_session_id_task = Arc::clone(&backend_session_id);

        tokio::spawn(async move {
            let steering_content = claude_steering_content(&config);
            let agent_identity = claude_agent_identity(&config);
            let (session, mut raw_events) = match ClaudeSession::spawn(
                &roots,
                None,
                &config.startup_mcp_servers,
                steering_content.as_deref(),
                agent_identity.as_ref(),
                config.resolved_spawn_config.tool_policy.clone(),
            )
            .await
            {
                Ok(value) => value,
                Err(err) => {
                    tracing::error!("Failed to spawn Claude resume session: {err}");
                    return;
                }
            };

            let handle = session.command_handle();
            let maybe_emitter = subagent_emitter_rx.borrow().clone();
            if let Some(emitter) = maybe_emitter {
                session.set_subagent_emitter(emitter).await;
            }
            let resolved_settings = resolve_session_settings(&config);
            let model_override = match resolved_settings.0.get("model") {
                Some(SessionSettingValue::String(value)) => Some(value.clone()),
                _ => None,
            };
            let effort_override = match resolved_settings.0.get("effort") {
                Some(SessionSettingValue::String(value)) => Some(value.clone()),
                _ => None,
            };
            if model_override.is_some() || effort_override.is_some() {
                let settings = json!({
                    "model": model_override,
                    "effort": effort_override,
                    "permission_mode": CLAUDE_DEFAULT_PERMISSION_MODE,
                });
                if let Err(err) = handle
                    .execute(SessionCommand::UpdateSettings {
                        settings,
                        persist: false,
                    })
                    .await
                {
                    tracing::error!("Failed to configure resumed Claude session: {err}");
                    session.shutdown().await;
                    return;
                }
            }

            if let Err(err) = handle
                .execute(SessionCommand::ResumeSession { session_id })
                .await
            {
                tracing::error!("Failed to resume Claude session: {err}");
                session.shutdown().await;
                return;
            }

            loop {
                tokio::select! {
                        incoming = raw_events.recv() => {
                            let Some(raw) = incoming else {
                                break;
                            };
                            if !forward_claude_backend_event(raw, &events_tx, &backend_session_id_task, None).await {
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
                                        tracing::error!("Failed to send Claude resume follow-up: {err}");
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
                                        tracing::error!("Failed to update resumed Claude session settings: {err}");
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
                                tracing::error!("Failed to interrupt resumed Claude turn: {err}");
                                break;
                            }
                        }
                        changed = subagent_emitter_rx.changed() => {
                            if changed.is_err() {
                                break;
                            }
                            let maybe_emitter = subagent_emitter_rx.borrow().clone();
                if let Some(emitter) = maybe_emitter {
                                session.set_subagent_emitter(emitter).await;
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
                subagent_emitter_tx,
            },
            EventStream::new(events_rx),
        ))
    }

    async fn list_sessions() -> Result<Vec<BackendSession>, String> {
        Err("ClaudeBackend::list_sessions is not supported without workspace context".to_string())
    }

    fn session_id(&self) -> SessionId {
        self.session_id
            .lock()
            .expect("claude session_id mutex poisoned")
            .clone()
            .expect("claude session_id not initialized")
    }

    async fn send(&self, input: AgentInput) -> bool {
        self.input_tx.send(input).await.is_ok()
    }

    async fn interrupt(&self) -> bool {
        self.interrupt_tx.send(()).await.is_ok()
    }

    async fn shutdown(self) {
        drop(self);
    }
}

/// Write a user message to the claude CLI stdin in stream-json format.
async fn signal_ready(ready_tx: &ClaudeReadyTx, result: Result<(), String>) {
    let mut ready_tx = ready_tx.lock().await;
    if let Some(tx) = ready_tx.take() {
        let _ = tx.send(result);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::oneshot;
    use tokio::time::{Duration, timeout};

    fn make_image(data: &str, media_type: &str) -> ImageAttachment {
        ImageAttachment {
            data: data.to_string(),
            media_type: media_type.to_string(),
            name: "image".to_string(),
            size: data.len() as u64,
        }
    }

    fn make_test_inner() -> (ClaudeInner, mpsc::UnboundedReceiver<Value>) {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let inner = ClaudeInner {
            event_tx,
            state: Mutex::new(ClaudeState {
                workspace_root: "/tmp/test-workspace".to_string(),
                ssh_host: None,
                session_id: None,
                ephemeral: false,
                model: None,
                effort: None,
                permission_mode: None,
                startup_mcp_config_json: None,
                steering_content: None,
                agent_identity: None,
                tool_policy: ToolPolicy::Unrestricted,
                last_cumulative_usage: None,
                conversation_bytes_total: 0,
                active_turn: None,
                subagent_emitter: None,
            }),
        };
        (inner, event_rx)
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct TestSubAgentSpawnRecord {
        tool_use_id: String,
        name: String,
        description: String,
        agent_type: String,
        session_id_hint: Option<protocol::SessionId>,
        agent_id: protocol::AgentId,
    }

    #[derive(Clone, Default)]
    struct TestSubAgentEmitter {
        next_id: Arc<AtomicU64>,
        spawns: Arc<std::sync::Mutex<Vec<TestSubAgentSpawnRecord>>>,
        event_receivers:
            Arc<std::sync::Mutex<HashMap<String, mpsc::UnboundedReceiver<protocol::ChatEvent>>>>,
    }

    impl TestSubAgentEmitter {
        fn spawn_records(&self) -> Vec<TestSubAgentSpawnRecord> {
            self.spawns.lock().expect("spawn record mutex").clone()
        }

        fn take_event_rx(&self, tool_use_id: &str) -> mpsc::UnboundedReceiver<protocol::ChatEvent> {
            self.event_receivers
                .lock()
                .expect("event receiver mutex")
                .remove(tool_use_id)
                .expect("sub-agent event receiver should exist")
        }
    }

    impl SubAgentEmitter for TestSubAgentEmitter {
        fn on_subagent_spawned(
            &self,
            tool_use_id: String,
            name: String,
            description: String,
            agent_type: String,
            session_id_hint: Option<protocol::SessionId>,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = SubAgentHandle> + Send + '_>>
        {
            let agent_id = protocol::AgentId(format!(
                "test-subagent-{}",
                self.next_id.fetch_add(1, Ordering::SeqCst)
            ));
            let (event_tx, event_rx) = mpsc::unbounded_channel();
            self.event_receivers
                .lock()
                .expect("event receiver mutex")
                .insert(tool_use_id.clone(), event_rx);
            self.spawns
                .lock()
                .expect("spawn record mutex")
                .push(TestSubAgentSpawnRecord {
                    tool_use_id,
                    name,
                    description,
                    agent_type,
                    session_id_hint,
                    agent_id: agent_id.clone(),
                });
            let _ = agent_id;
            Box::pin(async move { SubAgentHandle { event_tx } })
        }
    }

    fn event_kind(event: &Value) -> Option<&str> {
        event.get("kind").and_then(Value::as_str)
    }

    fn stream_end_message(event: &Value) -> &Value {
        event
            .get("data")
            .and_then(|data| data.get("message"))
            .expect("stream end message")
    }

    fn stream_end_tool_call_ids(event: &Value) -> Vec<String> {
        stream_end_message(event)
            .get("tool_calls")
            .and_then(Value::as_array)
            .expect("stream end tool_calls")
            .iter()
            .filter_map(|tool_call| {
                tool_call
                    .get("id")
                    .and_then(Value::as_str)
                    .map(|value| value.to_string())
            })
            .collect()
    }

    fn stream_end_total_tokens(event: &Value) -> Option<u64> {
        stream_end_message(event)
            .get("token_usage")
            .and_then(|usage| usage.get("total_tokens"))
            .and_then(Value::as_u64)
    }

    fn emit_test_phase_end(inner: &ClaudeInner, summary: &mut ClaudeStdoutSummary) {
        let phase = take_phase_emission(summary).expect("phase emission");
        let tool_calls = phase
            .tool_calls
            .iter()
            .map(|tool| {
                json!({
                    "id": tool.id,
                    "name": tool.name,
                    "arguments": tool.arguments,
                })
            })
            .collect::<Vec<_>>();
        inner.emit_stream_end(
            phase.text,
            phase.model,
            phase.usage,
            phase.reasoning,
            tool_calls,
            None,
        );
    }

    #[test]
    fn build_stream_json_user_message_includes_text_and_images() {
        let images = vec![
            make_image("base64-image", "image/jpeg"),
            make_image("   ", "image/png"),
        ];
        let payload = build_stream_json_user_message("hello", &images);
        let content = payload
            .get("message")
            .and_then(|message| message.get("content"))
            .and_then(Value::as_array)
            .expect("content blocks");

        assert_eq!(content.len(), 2);
        assert_eq!(content[0].get("type").and_then(Value::as_str), Some("text"));
        assert_eq!(
            content[0].get("text").and_then(Value::as_str),
            Some("hello")
        );
        assert_eq!(
            content[1].get("type").and_then(Value::as_str),
            Some("image")
        );
        assert_eq!(
            content[1]
                .get("source")
                .and_then(|source| source.get("media_type"))
                .and_then(Value::as_str),
            Some("image/jpeg")
        );
        assert_eq!(
            content[1]
                .get("source")
                .and_then(|source| source.get("data"))
                .and_then(Value::as_str),
            Some("base64-image")
        );
    }

    #[tokio::test]
    async fn emit_user_message_added_emits_user_message_with_images() {
        let (inner, mut rx) = make_test_inner();
        let images = vec![make_image("base64-image", "image/png")];

        inner.emit_user_message_added("hello", Some(&images));

        let event = rx.recv().await.expect("message event");
        assert_eq!(
            event.get("kind").and_then(Value::as_str),
            Some("MessageAdded")
        );
        assert_eq!(
            event
                .get("data")
                .and_then(|data| data.get("sender"))
                .and_then(Value::as_str),
            Some("User")
        );
        assert_eq!(
            event
                .get("data")
                .and_then(|data| data.get("content"))
                .and_then(Value::as_str),
            Some("hello")
        );
        let images = event
            .get("data")
            .and_then(|data| data.get("images"))
            .and_then(Value::as_array)
            .expect("images");
        assert_eq!(images.len(), 1);
        assert_eq!(
            images[0].get("media_type").and_then(Value::as_str),
            Some("image/png")
        );
        assert_eq!(
            images[0].get("data").and_then(Value::as_str),
            Some("base64-image")
        );
    }

    #[test]
    fn parse_claude_session_history_replays_tool_events_in_order() {
        let contents = format!(
            "{}\n{}\n",
            json!({
                "type": "assistant",
                "message": {
                    "role": "assistant",
                    "model": "claude-opus-4-6",
                    "content": [
                        { "type": "text", "text": "Running tool" },
                        { "type": "tool_use", "id": "toolu_1", "name": "Bash", "input": { "command": "ls -la" } }
                    ]
                }
            }),
            json!({
                "type": "user",
                "message": {
                    "role": "user",
                    "content": [
                        { "type": "tool_result", "tool_use_id": "toolu_1", "content": [{ "type": "text", "text": "ok" }] }
                    ]
                }
            })
        );

        let replay = parse_claude_session_history_contents(&contents);
        assert_eq!(replay.len(), 3);

        let assistant_message = match &replay[0] {
            ClaudeHistoryReplayItem::Message(message) => message,
            _ => panic!("first replay item should be assistant message"),
        };
        let message_tool_calls = assistant_message
            .get("tool_calls")
            .and_then(Value::as_array)
            .expect("message tool_calls");
        assert_eq!(message_tool_calls.len(), 1);
        assert_eq!(
            message_tool_calls[0].get("id").and_then(Value::as_str),
            Some("toolu_1")
        );

        let tool_request = match &replay[1] {
            ClaudeHistoryReplayItem::ToolRequest(tool_call) => tool_call,
            _ => panic!("second replay item should be tool request"),
        };
        assert_eq!(tool_request.id, "toolu_1");
        assert_eq!(tool_request.name, "Bash");

        let completion = match &replay[2] {
            ClaudeHistoryReplayItem::ToolExecutionCompleted(completion) => completion,
            _ => panic!("third replay item should be tool completion"),
        };
        assert!(completion.success);
        assert_eq!(completion.tool_call_id, "toolu_1");
        assert_eq!(completion.tool_name, "Bash");
        assert_eq!(
            completion.tool_result.get("kind").and_then(Value::as_str),
            Some("RunCommand")
        );
        assert_eq!(
            completion
                .tool_result
                .get("exit_code")
                .and_then(Value::as_i64),
            Some(0)
        );
        assert_eq!(
            completion.tool_result.get("stdout").and_then(Value::as_str),
            Some("ok")
        );
    }

    #[tokio::test]
    async fn top_level_assistant_boundaries_emit_separate_stream_ends_with_cumulative_usage() {
        let (inner, mut rx) = make_test_inner();
        let mut summary = ClaudeStdoutSummary::default();
        let mut segment = SegmentState::default();
        let base_id = "claude-msg-1".to_string();
        let mut current_id = base_id.clone();

        inner.emit_stream_start(&base_id, None);
        assert_eq!(
            event_kind(&rx.recv().await.expect("initial stream start")),
            Some("StreamStart")
        );

        consume_claude_stream_value(
            &json!({
                "type": "assistant",
                "message": {
                    "id": "assistant-msg-1",
                    "role": "assistant",
                    "model": "claude-opus-4-6",
                    "usage": {
                        "input_tokens": 100,
                        "output_tokens": 20,
                        "total_tokens": 120
                    },
                    "content": [
                        { "type": "text", "text": "First answer" }
                    ]
                }
            }),
            &mut summary,
            &mut segment,
            &inner,
            &base_id,
            &mut current_id,
        );
        assert!(rx.try_recv().is_err());

        consume_claude_stream_value(
            &json!({
                "type": "assistant",
                "message": {
                    "id": "assistant-msg-2",
                    "role": "assistant",
                    "model": "claude-opus-4-6",
                    "usage": {
                        "input_tokens": 250,
                        "output_tokens": 50,
                        "total_tokens": 300
                    },
                    "content": [
                        { "type": "text", "text": "Second answer" }
                    ]
                }
            }),
            &mut summary,
            &mut segment,
            &inner,
            &base_id,
            &mut current_id,
        );

        let first_end = rx.recv().await.expect("first stream end");
        assert_eq!(event_kind(&first_end), Some("StreamEnd"));
        assert_eq!(
            stream_end_message(&first_end)
                .get("content")
                .and_then(Value::as_str),
            Some("First answer")
        );
        assert_eq!(stream_end_total_tokens(&first_end), Some(120));

        let second_start = rx.recv().await.expect("second stream start");
        assert_eq!(event_kind(&second_start), Some("StreamStart"));
        assert_eq!(
            second_start
                .get("data")
                .and_then(|data| data.get("message_id"))
                .and_then(Value::as_str),
            Some("claude-msg-1-seg-1")
        );

        emit_test_phase_end(&inner, &mut summary);
        let second_end = rx.recv().await.expect("second stream end");
        assert_eq!(event_kind(&second_end), Some("StreamEnd"));
        assert_eq!(
            stream_end_message(&second_end)
                .get("content")
                .and_then(Value::as_str),
            Some("Second answer")
        );
        assert_eq!(stream_end_total_tokens(&second_end), Some(300));
    }

    #[tokio::test]
    async fn streamed_tool_use_emits_stream_end_then_tool_lifecycle_before_next_stream_start() {
        let (inner, mut rx) = make_test_inner();
        let mut summary = ClaudeStdoutSummary::default();
        let mut segment = SegmentState::default();
        let base_id = "claude-msg-1".to_string();
        let mut current_id = base_id.clone();

        inner.emit_stream_start(&base_id, None);
        assert_eq!(
            event_kind(&rx.recv().await.expect("initial stream start")),
            Some("StreamStart")
        );

        consume_claude_stream_value(
            &json!({
                "type": "stream_event",
                "event": {
                    "type": "message_start",
                    "message": {
                        "id": "assistant-msg-1",
                        "model": "claude-opus-4-6",
                        "usage": {
                            "input_tokens": 80,
                            "output_tokens": 0,
                            "total_tokens": 80
                        }
                    }
                }
            }),
            &mut summary,
            &mut segment,
            &inner,
            &base_id,
            &mut current_id,
        );

        consume_claude_stream_value(
            &json!({
                "type": "stream_event",
                "event": {
                    "type": "content_block_start",
                    "index": 0,
                    "content_block": {
                        "type": "tool_use",
                        "id": "toolu_edit",
                        "name": "Edit",
                        "input": {}
                    }
                }
            }),
            &mut summary,
            &mut segment,
            &inner,
            &base_id,
            &mut current_id,
        );
        assert!(rx.try_recv().is_err());

        consume_claude_stream_value(
            &json!({
                "type": "stream_event",
                "event": {
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": {
                        "type": "input_json_delta",
                        "partial_json": "{\"file_path\":\"/tmp/example.txt\",\"old_string\":\"old line\",\"new_string\":\"new line\"}"
                    }
                }
            }),
            &mut summary,
            &mut segment,
            &inner,
            &base_id,
            &mut current_id,
        );

        assert!(rx.try_recv().is_err());

        // content_block_stop finalises the tool_use block
        consume_claude_stream_value(
            &json!({
                "type": "stream_event",
                "event": {
                    "type": "content_block_stop",
                    "index": 0
                }
            }),
            &mut summary,
            &mut segment,
            &inner,
            &base_id,
            &mut current_id,
        );

        // message_stop closes the phase: StreamEnd (with tool calls) then ToolRequest
        consume_claude_stream_value(
            &json!({
                "type": "stream_event",
                "event": {
                    "type": "message_stop"
                }
            }),
            &mut summary,
            &mut segment,
            &inner,
            &base_id,
            &mut current_id,
        );

        let stream_end = rx.recv().await.expect("stream end at message_stop");
        assert_eq!(event_kind(&stream_end), Some("StreamEnd"));
        assert_eq!(
            stream_end_tool_call_ids(&stream_end),
            vec!["toolu_edit".to_string()]
        );
        assert_eq!(stream_end_total_tokens(&stream_end), Some(80));

        let tool_request = rx.recv().await.expect("tool request after stream end");
        assert_eq!(event_kind(&tool_request), Some("ToolRequest"));
        assert_eq!(
            tool_request
                .get("data")
                .and_then(|data| data.get("tool_type"))
                .and_then(|tool_type| tool_type.get("kind"))
                .and_then(Value::as_str),
            Some("ModifyFile")
        );
        assert_eq!(
            tool_request
                .get("data")
                .and_then(|data| data.get("tool_type"))
                .and_then(|tool_type| tool_type.get("file_path"))
                .and_then(Value::as_str),
            Some("/tmp/example.txt")
        );

        // Tool result arrives — phase already closed, only ToolExecutionCompleted
        consume_claude_stream_value(
            &json!({
                "type": "user",
                "message": {
                    "role": "user",
                    "content": [
                        {
                            "type": "tool_result",
                            "tool_use_id": "toolu_edit",
                            "content": "ok"
                        }
                    ]
                }
            }),
            &mut summary,
            &mut segment,
            &inner,
            &base_id,
            &mut current_id,
        );

        let completion = rx.recv().await.expect("tool completion event");
        assert_eq!(event_kind(&completion), Some("ToolExecutionCompleted"));
        assert_eq!(
            completion
                .get("data")
                .and_then(|data| data.get("tool_call_id"))
                .and_then(Value::as_str),
            Some("toolu_edit")
        );

        consume_claude_stream_value(
            &json!({
                "type": "stream_event",
                "event": {
                    "type": "message_start",
                    "message": {
                        "id": "assistant-msg-2",
                        "model": "claude-opus-4-6",
                        "usage": {
                            "input_tokens": 120,
                            "output_tokens": 20,
                            "total_tokens": 140
                        }
                    }
                }
            }),
            &mut summary,
            &mut segment,
            &inner,
            &base_id,
            &mut current_id,
        );

        let next_stream_start = rx.recv().await.expect("next stream start");
        assert_eq!(event_kind(&next_stream_start), Some("StreamStart"));
        assert_eq!(
            next_stream_start
                .get("data")
                .and_then(|data| data.get("message_id"))
                .and_then(Value::as_str),
            Some("claude-msg-1-seg-1")
        );
    }

    #[tokio::test]
    async fn wrapped_event_envelope_emits_reasoning_delta() {
        let (inner, mut rx) = make_test_inner();
        let mut summary = ClaudeStdoutSummary::default();
        let mut segment = SegmentState::default();
        let base_id = "claude-msg-1".to_string();
        let mut current_id = base_id.clone();

        consume_claude_stream_value(
            &json!({
                "type": "event",
                "event": "content_block_delta",
                "data": {
                    "index": 0,
                    "delta": {
                        "type": "thinking_delta",
                        "text": "Checking workspace constraints."
                    }
                }
            }),
            &mut summary,
            &mut segment,
            &inner,
            &base_id,
            &mut current_id,
        );

        assert_eq!(
            summary.best_reasoning(),
            Some("Checking workspace constraints.".to_string())
        );
        let event = rx.recv().await.expect("reasoning stream event");
        assert_eq!(
            event.get("kind").and_then(Value::as_str),
            Some("StreamReasoningDelta")
        );
        assert_eq!(
            event
                .get("data")
                .and_then(|data| data.get("text"))
                .and_then(Value::as_str),
            Some("Checking workspace constraints.")
        );
    }

    #[test]
    fn stream_event_with_string_name_is_consumed() {
        let (inner, _rx) = make_test_inner();
        let mut summary = ClaudeStdoutSummary::default();
        let mut segment = SegmentState::default();
        let base_id = "claude-msg-1".to_string();
        let mut current_id = base_id.clone();

        consume_claude_stream_value(
            &json!({
                "type": "stream_event",
                "event": "content_block_delta",
                "data": {
                    "index": 0,
                    "delta": {
                        "type": "text_delta",
                        "text": "hello"
                    }
                }
            }),
            &mut summary,
            &mut segment,
            &inner,
            &base_id,
            &mut current_id,
        );

        assert_eq!(summary.streamed_text, "hello");
    }

    #[test]
    fn collect_tool_use_blocks_handles_stream_event_string_envelope() {
        let payload = json!({
            "type": "stream_event",
            "event": "content_block_start",
            "data": {
                "index": 0,
                "content_block": {
                    "type": "tool_use",
                    "id": "toolu_spawn",
                    "name": "Task",
                    "input": { "description": "Spawn test sub-agent" }
                }
            }
        });

        let blocks = collect_tool_use_blocks(&payload);
        assert_eq!(blocks.len(), 1);
        assert_eq!(
            blocks[0].get("id").and_then(Value::as_str),
            Some("toolu_spawn")
        );
        assert_eq!(blocks[0].get("name").and_then(Value::as_str), Some("Task"));
    }

    #[test]
    fn direct_stream_event_type_is_consumed() {
        let (inner, _rx) = make_test_inner();
        let mut summary = ClaudeStdoutSummary::default();
        let mut segment = SegmentState::default();
        let base_id = "claude-msg-1".to_string();
        let mut current_id = base_id.clone();

        consume_claude_stream_value(
            &json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {
                    "type": "text_delta",
                    "text": "world"
                }
            }),
            &mut summary,
            &mut segment,
            &inner,
            &base_id,
            &mut current_id,
        );

        assert_eq!(summary.streamed_text, "world");
    }

    #[test]
    fn compact_boundary_system_event_is_recognized() {
        let (inner, _rx) = make_test_inner();
        let mut summary = ClaudeStdoutSummary::default();
        let mut segment = SegmentState::default();
        let base_id = "claude-msg-1".to_string();
        let mut current_id = base_id.clone();

        consume_claude_stream_value(
            &json!({
                "type": "system",
                "subtype": "compact_boundary",
                "compact_metadata": {
                    "pre_tokens": 1234,
                    "trigger": "manual"
                }
            }),
            &mut summary,
            &mut segment,
            &inner,
            &base_id,
            &mut current_id,
        );

        assert!(matches!(
            summary.control_event,
            Some(ClaudeControlEvent::ConversationCompacted)
        ));
    }

    #[test]
    fn task_started_system_event_is_recognized_without_affecting_root_summary() {
        let (inner, _rx) = make_test_inner();
        let mut summary = ClaudeStdoutSummary::default();
        let mut segment = SegmentState::default();
        let base_id = "claude-msg-1".to_string();
        let mut current_id = base_id.clone();

        consume_claude_stream_value(
            &json!({
                "type": "system",
                "subtype": "task_started",
                "description": "Explore remote host connection code",
                "prompt": "Trace the end-to-end flow",
                "session_id": "test-session",
                "task_id": "task-123",
                "task_type": "local_agent",
                "tool_use_id": "toolu_123"
            }),
            &mut summary,
            &mut segment,
            &inner,
            &base_id,
            &mut current_id,
        );

        assert_eq!(summary.session_id.as_deref(), Some("test-session"));
        assert!(summary.control_event.is_none());
    }

    #[tokio::test]
    async fn task_started_local_agent_registers_subagent_and_routes_parent_events() {
        let emitter = TestSubAgentEmitter::default();
        let mut streams = HashMap::new();

        detect_subagent_task_system_spawns(
            &json!({
                "type": "system",
                "subtype": "task_started",
                "description": "Explore remote host connection code",
                "prompt": "Trace the end-to-end flow",
                "session_id": "test-session",
                "task_id": "task-123",
                "task_type": "local_agent",
                "tool_use_id": "toolu_123"
            }),
            &emitter,
            &mut streams,
        )
        .await;

        let spawn_records = emitter.spawn_records();
        assert_eq!(spawn_records.len(), 1);
        assert_eq!(spawn_records[0].tool_use_id, "toolu_123");
        assert_eq!(spawn_records[0].name, "Explore remote host connection code");
        assert_eq!(spawn_records[0].description, "Trace the end-to-end flow");
        assert_eq!(spawn_records[0].agent_type, "local_agent");
        assert!(streams.contains_key("toolu_123"));

        let mut child_events = emitter.take_event_rx("toolu_123");
        let prompt_event = timeout(Duration::from_millis(500), child_events.recv())
            .await
            .expect("task prompt event should arrive")
            .expect("task prompt chat event");
        let protocol::ChatEvent::MessageAdded(prompt_message) = prompt_event else {
            panic!("expected initial child MessageAdded event");
        };
        assert_eq!(prompt_message.content, "Trace the end-to-end flow");

        let stream = streams.get_mut("toolu_123").expect("sub-agent stream");
        consume_subagent_event(
            stream,
            &json!({
                "type": "content_block_start",
                "parent_tool_use_id": "toolu_123",
                "index": 0,
                "content_block": {
                    "type": "text",
                    "text": "child says hello"
                }
            }),
        );

        let stream_start = timeout(Duration::from_millis(500), child_events.recv())
            .await
            .expect("child stream start should arrive")
            .expect("child stream start event");
        let protocol::ChatEvent::StreamStart(start) = stream_start else {
            panic!("expected child StreamStart event");
        };
        assert!(
            start.message_id.is_some(),
            "child stream start should carry a message id"
        );

        let stream_delta = timeout(Duration::from_millis(500), child_events.recv())
            .await
            .expect("child stream delta should arrive")
            .expect("child stream delta event");
        let protocol::ChatEvent::StreamDelta(delta) = stream_delta else {
            panic!("expected child StreamDelta event");
        };
        assert_eq!(delta.text, "child says hello");

        detect_subagent_completions(
            &json!({
                "type": "user",
                "message": {
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": "toolu_123",
                        "is_error": false,
                        "content": "child final response"
                    }]
                }
            }),
            &mut streams,
        )
        .await;

        let stream_end = timeout(Duration::from_millis(500), child_events.recv())
            .await
            .expect("child stream end should arrive")
            .expect("child stream end event");
        let protocol::ChatEvent::StreamEnd(end) = stream_end else {
            panic!("expected child StreamEnd event");
        };
        assert_eq!(end.message.content, "child says hello");

        assert!(
            streams.is_empty(),
            "sub-agent stream should be removed after tool_result completion"
        );
    }

    #[tokio::test]
    async fn task_started_local_bash_does_not_register_subagent() {
        let emitter = TestSubAgentEmitter::default();
        let mut streams = HashMap::new();

        detect_subagent_task_system_spawns(
            &json!({
                "type": "system",
                "subtype": "task_started",
                "description": "Run git status",
                "prompt": "Run git status in the repo",
                "task_id": "task-bash",
                "task_type": "local_bash",
                "tool_use_id": "toolu_bash"
            }),
            &emitter,
            &mut streams,
        )
        .await;

        assert!(streams.is_empty());
        assert!(emitter.spawn_records().is_empty());
    }

    #[tokio::test]
    async fn task_started_dedupes_with_later_tool_use_spawn() {
        let emitter = TestSubAgentEmitter::default();
        let mut streams = HashMap::new();
        let mut pending_prompts = HashMap::new();

        detect_subagent_task_system_spawns(
            &json!({
                "type": "system",
                "subtype": "task_started",
                "description": "Explore remote host connection code",
                "prompt": "Trace the end-to-end flow",
                "task_id": "task-123",
                "task_type": "local_agent",
                "tool_use_id": "toolu_123"
            }),
            &emitter,
            &mut streams,
        )
        .await;

        let mut child_events = emitter.take_event_rx("toolu_123");
        let prompt_event = timeout(Duration::from_millis(500), child_events.recv())
            .await
            .expect("initial task prompt event should arrive")
            .expect("initial task prompt chat event");
        let protocol::ChatEvent::MessageAdded(prompt_message) = prompt_event else {
            panic!("expected initial child MessageAdded event");
        };
        assert_eq!(prompt_message.content, "Trace the end-to-end flow");

        detect_subagent_spawns(
            &json!({
                "type": "assistant",
                "message": {
                    "content": [{
                        "type": "tool_use",
                        "id": "toolu_123",
                        "name": "Task",
                        "input": {
                            "description": "Explore remote host connection code",
                            "prompt": "Trace the end-to-end flow",
                            "subagent_type": "local_agent"
                        }
                    }]
                }
            }),
            &emitter,
            &mut streams,
            &mut pending_prompts,
        )
        .await;

        assert_eq!(
            emitter.spawn_records().len(),
            1,
            "tool_use fallback should reuse the task_started registration"
        );
        assert_eq!(streams.len(), 1);
        assert!(
            timeout(Duration::from_millis(100), child_events.recv())
                .await
                .is_err(),
            "deduped tool_use spawn should not emit a duplicate prompt"
        );
    }

    #[tokio::test]
    async fn compact_boundary_emits_visible_system_message_and_stream_end() {
        let (inner, mut rx) = make_test_inner();
        let mut summary = ClaudeStdoutSummary {
            control_event: Some(ClaudeControlEvent::ConversationCompacted),
            ..ClaudeStdoutSummary::default()
        };

        let emitted = inner
            .emit_terminal_phase_or_placeholder(&mut summary, 0, None, None)
            .await;
        assert!(
            emitted,
            "compact boundary should count as a recognized completion"
        );

        let message_added = rx.recv().await.expect("system message");
        assert_eq!(event_kind(&message_added), Some("MessageAdded"));
        assert_eq!(
            message_added
                .get("data")
                .and_then(|data| data.get("sender"))
                .and_then(Value::as_str),
            Some("System")
        );
        assert_eq!(
            message_added
                .get("data")
                .and_then(|data| data.get("content"))
                .and_then(Value::as_str),
            Some("Conversation compacted.")
        );

        let stream_end = rx.recv().await.expect("stream end");
        assert_eq!(event_kind(&stream_end), Some("StreamEnd"));
    }

    fn make_live_test_inner(
        workspace_root: String,
    ) -> (Arc<ClaudeInner>, mpsc::UnboundedReceiver<Value>) {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        (
            Arc::new(ClaudeInner {
                event_tx,
                state: Mutex::new(ClaudeState {
                    workspace_root,
                    ssh_host: None,
                    session_id: None,
                    ephemeral: true,
                    model: None,
                    effort: Some("high".to_string()),
                    permission_mode: Some(CLAUDE_DEFAULT_PERMISSION_MODE.to_string()),
                    startup_mcp_config_json: None,
                    steering_content: None,
                    agent_identity: None,
                    tool_policy: ToolPolicy::Unrestricted,
                    last_cumulative_usage: None,
                    conversation_bytes_total: 0,
                    active_turn: None,
                    subagent_emitter: None,
                }),
            }),
            event_rx,
        )
    }

    async fn run_live_claude_turn(
        prompt: &str,
        effort: &str,
        session_id: Option<String>,
        ephemeral: bool,
    ) -> TurnOutcome {
        let workspace_root = std::env::var("TYDE_CLAUDE_TEST_WORKSPACE")
            .unwrap_or_else(|_| env!("CARGO_MANIFEST_DIR").to_string());
        let (inner, _rx) = make_live_test_inner(workspace_root.clone());
        let (cancel_tx, cancel_rx) = oneshot::channel();
        let outcome = inner
            .run_turn(
                RunTurnParams {
                    message_id: "claude-live-integration-msg-1",
                    workspace_root: &workspace_root,
                    ssh_host: None,
                    prompt,
                    images: &[],
                    session_id,
                    ephemeral,
                    model: None,
                    effort: Some(effort.to_string()),
                    permission_mode: Some(CLAUDE_DEFAULT_PERMISSION_MODE.to_string()),
                    startup_mcp_config_json: None,
                    steering_content: None,
                    agent_identity: None,
                    tool_policy: ToolPolicy::Unrestricted,
                },
                cancel_rx,
            )
            .await;
        drop(cancel_tx);
        outcome
    }

    fn live_test_workspace_root() -> String {
        std::env::var("TYDE_CLAUDE_TEST_WORKSPACE")
            .unwrap_or_else(|_| env!("CARGO_MANIFEST_DIR").to_string())
    }

    fn format_live_events(events: &[Value]) -> String {
        serde_json::to_string_pretty(&Value::Array(events.to_vec()))
            .unwrap_or_else(|_| format!("{events:?}"))
    }

    async fn collect_live_claude_events(prompt: &str, workspace_root: String) -> Vec<Value> {
        let (inner, mut rx) = make_live_test_inner(workspace_root);
        inner.clone().start_turn(prompt.to_string(), None).await;

        let mut events = Vec::new();
        loop {
            let event = timeout(Duration::from_secs(180), rx.recv())
                .await
                .expect("timed out waiting for live Claude event")
                .expect("live Claude event channel closed");
            let is_done = event_kind(&event) == Some("TypingStatusChanged")
                && event.get("data").and_then(Value::as_bool) == Some(false);
            events.push(event);
            if is_done {
                break;
            }
        }

        events
    }

    #[tokio::test]
    #[ignore = "requires local Claude CLI auth and network; set TYDE_RUN_CLAUDE_INTEGRATION=1"]
    async fn live_claude_turn_succeeds_at_high_effort() {
        if std::env::var("TYDE_RUN_CLAUDE_INTEGRATION").ok().as_deref() != Some("1") {
            eprintln!("Skipping live Claude integration test; set TYDE_RUN_CLAUDE_INTEGRATION=1");
            return;
        }

        let prompt = "Think carefully about this arithmetic problem and provide the final numeric answer at the end: (37 * 29) + 14.";
        match run_live_claude_turn(prompt, "high", None, true).await {
            TurnOutcome::Completed { summary, .. } => {
                assert!(
                    !summary.best_text().trim().is_empty(),
                    "Expected non-empty assistant text from live Claude turn"
                );
                assert!(
                    summary.usage.is_some() || summary.result_cumulative_usage.is_some(),
                    "Expected token usage from live Claude turn at high effort"
                );
            }
            TurnOutcome::Failed { error, .. } => {
                panic!("Live Claude integration turn failed: {error}");
            }
            TurnOutcome::Cancelled { .. } => {
                panic!("Live Claude integration turn was unexpectedly cancelled");
            }
        }
    }

    #[tokio::test]
    #[ignore = "requires local Claude CLI auth and network; set TYDE_RUN_CLAUDE_INTEGRATION=1"]
    async fn live_claude_resume_tracks_per_turn_and_cumulative_usage() {
        if std::env::var("TYDE_RUN_CLAUDE_INTEGRATION").ok().as_deref() != Some("1") {
            eprintln!("Skipping live Claude integration test; set TYDE_RUN_CLAUDE_INTEGRATION=1");
            return;
        }

        let first = run_live_claude_turn(
            "Briefly explain what a Rust borrow checker does.",
            "high",
            None,
            false,
        )
        .await;
        let first_summary = match first {
            TurnOutcome::Completed { summary, .. } => summary,
            TurnOutcome::Failed { error, .. } => {
                panic!("Initial live Claude resume turn failed: {error}");
            }
            TurnOutcome::Cancelled { .. } => {
                panic!("Initial live Claude resume turn was unexpectedly cancelled");
            }
        };

        let session_id = first_summary
            .session_id
            .clone()
            .filter(|id| !id.trim().is_empty())
            .expect("Expected Claude to return a non-empty session_id for resume test");

        let second = run_live_claude_turn(
            "Now in one sentence summarize your previous answer.",
            "high",
            Some(session_id.clone()),
            false,
        )
        .await;
        let second_summary = match second {
            TurnOutcome::Completed { summary, .. } => summary,
            TurnOutcome::Failed { error, .. } => {
                panic!("Follow-up live Claude resume turn failed: {error}");
            }
            TurnOutcome::Cancelled { .. } => {
                panic!("Follow-up live Claude resume turn was unexpectedly cancelled");
            }
        };

        assert_eq!(
            second_summary.session_id.as_deref(),
            Some(session_id.as_str()),
            "Expected resumed turn to keep the same Claude session_id"
        );

        assert!(
            !second_summary.best_text().trim().is_empty(),
            "Expected non-empty assistant text on resumed Claude turn at high effort"
        );

        let turn_total = second_summary
            .usage
            .as_ref()
            .and_then(|usage| usage.get("total_tokens"))
            .and_then(Value::as_u64)
            .expect("Expected per-turn usage.total_tokens on resumed turn");
        let cumulative_total = second_summary
            .result_cumulative_usage
            .as_ref()
            .and_then(|usage| usage.get("total_tokens"))
            .and_then(Value::as_u64)
            .expect("Expected cumulative usage.total_tokens from result event");
        assert!(
            cumulative_total >= turn_total,
            "Expected cumulative session usage ({cumulative_total}) to be >= per-turn usage ({turn_total})"
        );
    }

    #[tokio::test]
    #[ignore = "requires local Claude CLI auth and network; set TYDE_RUN_CLAUDE_INTEGRATION=1"]
    async fn live_claude_tool_turn_emits_stream_end_before_tool_events() {
        if std::env::var("TYDE_RUN_CLAUDE_INTEGRATION").ok().as_deref() != Some("1") {
            eprintln!("Skipping live Claude integration test; set TYDE_RUN_CLAUDE_INTEGRATION=1");
            return;
        }

        let workspace_root = live_test_workspace_root();
        let marker = format!("claude-live-tool-marker-{}", unix_now_ms());
        let file_name = format!(".tyde-live-tool-order-{}.txt", unix_now_ms());
        let file_path = Path::new(&workspace_root).join(&file_name);
        tokio_fs::write(&file_path, format!("{marker}\n"))
            .await
            .expect("write live tool-order fixture file");

        let prompt = format!(
            "Use the Read tool exactly once to read the file `{file_name}` in the current working directory. Do not guess or answer from memory. After the tool finishes, reply with only the exact file contents."
        );
        let events = collect_live_claude_events(&prompt, workspace_root.clone()).await;
        let _ = tokio_fs::remove_file(&file_path).await;
        let events_dump = format_live_events(&events);

        let stream_end_index = events
            .iter()
            .position(|event| {
                event_kind(event) == Some("StreamEnd")
                    && !stream_end_tool_call_ids(event).is_empty()
            })
            .unwrap_or_else(|| {
                panic!(
                    "Expected a StreamEnd with tool_calls in live Claude tool turn. Events:\n{events_dump}"
                )
            });
        let tool_call_ids = stream_end_tool_call_ids(&events[stream_end_index]);
        let first_tool_call_id = tool_call_ids
            .first()
            .cloned()
            .expect("Expected at least one live tool call id");
        let tool_request_index = events
            .iter()
            .position(|event| {
                event_kind(event) == Some("ToolRequest")
                    && event
                        .get("data")
                        .and_then(|data| data.get("tool_call_id"))
                        .and_then(Value::as_str)
                        == Some(first_tool_call_id.as_str())
            })
            .unwrap_or_else(|| {
                panic!("Expected ToolRequest for live Claude tool call. Events:\n{events_dump}")
            });
        let completion_index = events
            .iter()
            .position(|event| {
                event_kind(event) == Some("ToolExecutionCompleted")
                    && event
                        .get("data")
                        .and_then(|data| data.get("tool_call_id"))
                        .and_then(Value::as_str)
                        == Some(first_tool_call_id.as_str())
            })
            .unwrap_or_else(|| {
                panic!(
                    "Expected ToolExecutionCompleted for live Claude tool call. Events:\n{events_dump}"
                )
            });
        let next_stream_start_index = events
            .iter()
            .enumerate()
            .skip(completion_index + 1)
            .find_map(|(index, event)| {
                (event_kind(event) == Some("StreamStart")).then_some(index)
            })
            .unwrap_or_else(|| {
                panic!(
                    "Expected a follow-up StreamStart after live tool completion. Events:\n{events_dump}"
                )
            });

        assert!(
            stream_end_index < tool_request_index,
            "Expected StreamEnd with tool_calls to occur before ToolRequest. Events:\n{events_dump}"
        );
        assert!(
            tool_request_index < completion_index,
            "Expected ToolRequest to occur before ToolExecutionCompleted. Events:\n{events_dump}"
        );
        assert!(
            completion_index < next_stream_start_index,
            "Expected next StreamStart after ToolExecutionCompleted. Events:\n{events_dump}"
        );
    }

    #[test]
    fn parse_claude_session_replay_restores_result_usage_and_history_bytes() {
        let contents = format!(
            "{}\n{}\n{}\n",
            json!({
                "type": "user",
                "message": {
                    "role": "user",
                    "content": [{ "type": "text", "text": "Question" }]
                }
            }),
            json!({
                "type": "assistant",
                "message": {
                    "role": "assistant",
                    "content": [{ "type": "text", "text": "Answer" }]
                }
            }),
            json!({
                "type": "result",
                "usage": {
                    "input_tokens": 1200,
                    "output_tokens": 80,
                    "total_tokens": 1280,
                    "cache_read_input_tokens": 5000
                }
            })
        );

        let replay = parse_claude_session_replay(&contents);
        assert_eq!(replay.items.len(), 2);
        assert_eq!(
            replay.conversation_bytes_total,
            "Question".len() as u64 + "Answer".len() as u64
        );
        let usage = replay
            .last_cumulative_usage
            .as_ref()
            .expect("last cumulative usage");
        assert_eq!(
            usage.get("input_tokens").and_then(Value::as_u64),
            Some(1200)
        );
        assert_eq!(
            usage.get("cached_prompt_tokens").and_then(Value::as_u64),
            Some(5000)
        );
    }

    #[test]
    fn extract_reasoning_from_message_accepts_reasoning_summary_blocks() {
        let message = json!({
            "content": [
                { "type": "text", "text": "visible answer" },
                { "type": "reasoning_summary", "summary": "Checking constraints first." }
            ]
        });

        assert_eq!(
            extract_reasoning_from_message(&message),
            Some("Checking constraints first.".to_string())
        );
    }

    #[test]
    fn extract_reasoning_text_accepts_camel_case_summary_fields() {
        let block = json!({
            "type": "reasoning_summary",
            "summaryText": "Tracing Claude summary output."
        });

        assert_eq!(
            extract_reasoning_text(&block),
            Some("Tracing Claude summary output.".to_string())
        );
    }

    #[test]
    fn extract_reasoning_text_preserves_boundary_whitespace() {
        assert_eq!(
            extract_reasoning_text(&json!(" user")),
            Some(" user".to_string())
        );
    }

    #[test]
    fn append_reasoning_text_preserves_leading_spaces_between_deltas() {
        let mut summary = ClaudeStdoutSummary::default();

        append_reasoning_text(&mut summary, "The", false);
        append_reasoning_text(&mut summary, " user", false);

        assert_eq!(summary.streamed_reasoning, "The user");
    }

    #[test]
    fn extract_tool_result_events_maps_success_and_error_results() {
        let message = json!({
            "content": [
                {
                    "type": "tool_result",
                    "tool_use_id": "toolu_ok",
                    "content": "command output"
                },
                {
                    "type": "tool_result",
                    "tool_use_id": "toolu_fail",
                    "is_error": true,
                    "content": "stderr:\npermission denied\nexit code: 126"
                }
            ]
        });
        let mut tool_names = HashMap::new();
        tool_names.insert("toolu_ok".to_string(), "ReadFiles".to_string());
        tool_names.insert("toolu_fail".to_string(), "Bash".to_string());
        let tool_calls = HashMap::new();

        let events = extract_tool_result_events_from_message(&message, &tool_names, &tool_calls);
        assert_eq!(events.len(), 2);

        let success = &events[0];
        assert!(success.success);
        assert_eq!(success.tool_call_id, "toolu_ok");
        assert_eq!(success.tool_name, "ReadFiles");
        assert!(success.error.is_none());
        assert_eq!(
            success.tool_result.get("kind").and_then(Value::as_str),
            Some("Other")
        );

        let failure = &events[1];
        assert!(!failure.success);
        assert_eq!(failure.tool_call_id, "toolu_fail");
        assert_eq!(failure.tool_name, "Bash");
        assert_eq!(
            failure.tool_result.get("kind").and_then(Value::as_str),
            Some("RunCommand")
        );
        assert_eq!(
            failure.tool_result.get("exit_code").and_then(Value::as_i64),
            Some(126)
        );
        assert_eq!(
            failure.tool_result.get("stderr").and_then(Value::as_str),
            Some("permission denied\nexit code: 126")
        );
        assert!(failure.error.is_some());
    }

    #[test]
    fn extract_tool_result_events_maps_bash_success_to_run_command() {
        let message = json!({
            "content": [
                {
                    "type": "tool_result",
                    "tool_use_id": "toolu_bash",
                    "content": "command output line"
                }
            ]
        });
        let mut tool_names = HashMap::new();
        tool_names.insert("toolu_bash".to_string(), "Bash".to_string());
        let tool_calls = HashMap::new();

        let events = extract_tool_result_events_from_message(&message, &tool_names, &tool_calls);
        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert!(event.success);
        assert_eq!(
            event.tool_result.get("kind").and_then(Value::as_str),
            Some("RunCommand")
        );
        assert_eq!(
            event.tool_result.get("stdout").and_then(Value::as_str),
            Some("command output line")
        );
        assert_eq!(
            event.tool_result.get("stderr").and_then(Value::as_str),
            Some("")
        );
        assert_eq!(
            event.tool_result.get("exit_code").and_then(Value::as_i64),
            Some(0)
        );
    }

    #[test]
    fn claude_tool_request_type_maps_edit_to_modify_file() {
        let request = claude_tool_request_type(
            "Edit",
            &json!({
                "file_path": "/tmp/example.txt",
                "old_string": "old line",
                "new_string": "new line"
            }),
        );

        assert_eq!(
            request.get("kind").and_then(Value::as_str),
            Some("ModifyFile")
        );
        assert_eq!(
            request.get("file_path").and_then(Value::as_str),
            Some("/tmp/example.txt")
        );
        assert_eq!(
            request.get("before").and_then(Value::as_str),
            Some("old line")
        );
        assert_eq!(
            request.get("after").and_then(Value::as_str),
            Some("new line")
        );
    }

    #[test]
    fn claude_tool_request_type_maps_read_to_read_files() {
        let request = claude_tool_request_type(
            "Read",
            &json!({
                "file_path": "/tmp/example.txt"
            }),
        );

        assert_eq!(
            request.get("kind").and_then(Value::as_str),
            Some("ReadFiles")
        );
        let paths = request
            .get("file_paths")
            .and_then(Value::as_array)
            .expect("file_paths");
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].as_str(), Some("/tmp/example.txt"));
    }

    #[test]
    fn extract_tool_result_events_maps_modify_file_result() {
        let message = json!({
            "content": [
                {
                    "type": "tool_result",
                    "tool_use_id": "toolu_edit",
                    "content": "ok"
                }
            ]
        });

        let mut tool_names = HashMap::new();
        tool_names.insert("toolu_edit".to_string(), "Edit".to_string());

        let mut tool_calls = HashMap::new();
        tool_calls.insert(
            "toolu_edit".to_string(),
            ClaudeToolCall {
                id: "toolu_edit".to_string(),
                name: "Edit".to_string(),
                arguments: json!({
                    "file_path": "/tmp/example.txt",
                    "old_string": "old line",
                    "new_string": "new line"
                }),
            },
        );

        let events = extract_tool_result_events_from_message(&message, &tool_names, &tool_calls);
        assert_eq!(events.len(), 1);

        let event = &events[0];
        assert!(event.success);
        assert_eq!(event.tool_name, "Edit");
        assert_eq!(
            event.tool_result.get("kind").and_then(Value::as_str),
            Some("ModifyFile")
        );
        assert_eq!(
            event.tool_result.get("lines_added").and_then(Value::as_u64),
            Some(1)
        );
        assert_eq!(
            event
                .tool_result
                .get("lines_removed")
                .and_then(Value::as_u64),
            Some(1)
        );
    }

    #[test]
    fn derive_turn_token_usage_subtracts_prior_cumulative_usage() {
        let previous = json!({
            "input_tokens": 1_000,
            "output_tokens": 200,
            "total_tokens": 1_200,
            "cached_prompt_tokens": 800,
            "cache_creation_input_tokens": 50,
            "reasoning_tokens": 40,
            "context_window": 200_000
        });
        let current = json!({
            "input_tokens": 1_300,
            "output_tokens": 260,
            "total_tokens": 1_560,
            "cached_prompt_tokens": 1_020,
            "cache_creation_input_tokens": 70,
            "reasoning_tokens": 55,
            "context_window": 200_000
        });

        let turn = derive_turn_token_usage(&current, Some(&previous)).expect("turn usage");
        assert_eq!(
            turn,
            json!({
                "input_tokens": 300,
                "output_tokens": 60,
                "total_tokens": 360,
                "cached_prompt_tokens": 220,
                "cache_creation_input_tokens": 20,
                "reasoning_tokens": 15,
                "context_window": 200_000
            })
        );
    }

    #[test]
    fn derive_turn_token_usage_handles_counter_reset() {
        let previous = json!({
            "input_tokens": 10_000,
            "output_tokens": 1_000,
            "total_tokens": 11_000,
            "cached_prompt_tokens": 9_000,
            "cache_creation_input_tokens": 200,
            "reasoning_tokens": 400,
            "context_window": 200_000
        });
        let current = json!({
            "input_tokens": 220,
            "output_tokens": 30,
            "total_tokens": 250,
            "cached_prompt_tokens": 150,
            "cache_creation_input_tokens": 10,
            "reasoning_tokens": 8,
            "context_window": 200_000
        });

        let turn = derive_turn_token_usage(&current, Some(&previous)).expect("turn usage");
        assert_eq!(turn, current);
    }

    #[test]
    fn normalize_claude_permission_mode_maps_default_to_bypass_permissions() {
        let value = Value::String("default".to_string());
        assert_eq!(
            normalize_claude_permission_mode(&value),
            Some("bypassPermissions".to_string())
        );
    }

    #[test]
    fn normalize_claude_effort_accepts_max() {
        let value = Value::String("max".to_string());
        assert_eq!(normalize_claude_effort(&value), Some("max".to_string()));
    }

    #[test]
    fn parse_token_usage_accepts_camel_case_fields() {
        let usage = json!({
            "inputTokens": 1200,
            "outputTokens": 90,
            "totalTokens": 1290,
            "cacheReadInputTokens": 300,
            "cacheCreationInputTokens": 20,
            "reasoningTokens": 7,
            "contextWindow": 200_000
        });

        let parsed = parse_token_usage(Some(&usage)).expect("usage should parse");
        assert_eq!(
            parsed,
            json!({
                "input_tokens": 1200,
                "output_tokens": 90,
                "total_tokens": 1290,
                "cached_prompt_tokens": 300,
                "cache_creation_input_tokens": 20,
                "reasoning_tokens": 7,
                "context_window": 200_000
            })
        );
    }

    #[test]
    fn result_event_reasoning_summary_is_captured() {
        let (inner, _rx) = make_test_inner();
        let mut summary = ClaudeStdoutSummary::default();
        let mut segment = SegmentState::default();
        let base_id = "claude-msg-1".to_string();
        let mut current_id = base_id.clone();

        consume_claude_stream_value(
            &json!({
                "type": "result",
                "result": "Done",
                "reasoningSummaryText": "I validated the constraints first."
            }),
            &mut summary,
            &mut segment,
            &inner,
            &base_id,
            &mut current_id,
        );

        assert_eq!(
            summary.best_reasoning(),
            Some("I validated the constraints first.".to_string())
        );
        assert!(summary.reasoning_bytes > 0);
    }

    #[test]
    fn estimate_context_breakdown_uses_known_context_window() {
        let usage = json!({
            "input_tokens": 10,
            "output_tokens": 50,
            "total_tokens": 60,
            "cached_prompt_tokens": 18_000,
            "cache_creation_input_tokens": 2_000,
            "reasoning_tokens": 0
        });
        // With known_context_window = 200_000, context_window should be 200_000
        // even though input_tokens (10+18000+2000 = 20_010) < 200_000.
        let bd = estimate_context_breakdown(Some(&usage), 100, 50, 0, Some(200_000), None);
        assert_eq!(
            bd.get("context_window").and_then(Value::as_u64),
            Some(200_000)
        );
        assert_eq!(bd.get("input_tokens").and_then(Value::as_u64), Some(20_010));
    }

    #[test]
    fn estimate_context_breakdown_known_window_not_inflated_by_large_input() {
        // Regression: previously, if input_tokens exceeded the estimated context
        // window (200K), the fallback used max(200K, input_tokens) which inflated
        // context_window to match, always showing 100% utilization.
        let usage = json!({
            "input_tokens": 50_000,
            "output_tokens": 100,
            "total_tokens": 50_100,
            "cached_prompt_tokens": 400_000,
            "cache_creation_input_tokens": 150_000,
            "reasoning_tokens": 0
        });
        // Total prompt = 50K + 400K + 150K = 600K (bogus — would only happen
        // with the old cumulative-vs-per-call bug, but test the fallback anyway).
        // With known_context_window = 200_000, context_window stays at 200_000.
        let bd = estimate_context_breakdown(Some(&usage), 0, 0, 0, Some(200_000), None);
        assert_eq!(
            bd.get("context_window").and_then(Value::as_u64),
            Some(200_000)
        );
    }

    #[test]
    fn estimate_context_breakdown_falls_back_to_estimated_without_known_window() {
        let usage = json!({
            "input_tokens": 5,
            "output_tokens": 10,
            "total_tokens": 15,
            "cached_prompt_tokens": 20_000,
            "cache_creation_input_tokens": 0,
            "reasoning_tokens": 0
        });
        // No known_context_window → should fall back to the conservative default.
        let bd = estimate_context_breakdown(Some(&usage), 0, 0, 0, None, None);
        assert_eq!(
            bd.get("context_window").and_then(Value::as_u64),
            Some(CLAUDE_ESTIMATED_CONTEXT_WINDOW_DEFAULT)
        );
    }

    #[test]
    fn result_event_does_not_overwrite_per_api_call_usage() {
        // Simulate the event sequence from a multi-tool-call Claude process:
        // 1. assistant message with per-API-call usage
        // 2. result event with cumulative usage
        // summary.usage should remain the per-API-call value.
        let mut summary = ClaudeStdoutSummary {
            usage: Some(json!({
                "input_tokens": 1,
                "output_tokens": 5,
                "total_tokens": 6,
                "cached_prompt_tokens": 20_000,
                "cache_creation_input_tokens": 500,
                "reasoning_tokens": 0
            })),
            ..Default::default()
        };

        // Simulate consuming a result event — it should set
        // result_cumulative_usage, NOT overwrite usage.
        let result_event = json!({
            "type": "result",
            "result": "Done",
            "usage": {
                "input_tokens": 5,
                "output_tokens": 20,
                "total_tokens": 25,
                "cache_read_input_tokens": 40_000,
                "cache_creation_input_tokens": 1_000,
            },
            "modelUsage": {
                "claude-opus-4-6": {
                    "inputTokens": 5,
                    "outputTokens": 20,
                    "cacheReadInputTokens": 40_000,
                    "cacheCreationInputTokens": 1_000,
                    "contextWindow": 200_000
                }
            }
        });
        let (inner, _rx) = make_test_inner();
        let mut segment = SegmentState::default();
        let base_id = "test-msg".to_string();
        let mut current_id = base_id.clone();
        consume_claude_stream_value(
            &result_event,
            &mut summary,
            &mut segment,
            &inner,
            &base_id,
            &mut current_id,
        );

        // usage should still be the per-API-call value (not the cumulative)
        let usage = summary.usage.as_ref().expect("usage should be set");
        assert_eq!(usage.get("input_tokens").and_then(Value::as_u64), Some(1));
        assert_eq!(
            usage.get("cached_prompt_tokens").and_then(Value::as_u64),
            Some(20_000)
        );

        // result_cumulative_usage should hold the cumulative from result
        let cum = summary
            .result_cumulative_usage
            .as_ref()
            .expect("result_cumulative_usage should be set");
        assert_eq!(cum.get("input_tokens").and_then(Value::as_u64), Some(5));
        assert_eq!(
            cum.get("cached_prompt_tokens").and_then(Value::as_u64),
            Some(40_000)
        );

        // result_context_window should be extracted from modelUsage
        assert_eq!(summary.result_context_window, Some(200_000));
    }

    #[test]
    fn result_event_prefers_model_usage_entry_for_current_model() {
        let mut summary = ClaudeStdoutSummary {
            model: Some("claude-haiku-4-5-20251001".to_string()),
            ..Default::default()
        };

        let result_event = json!({
            "type": "result",
            "result": "Done",
            "model": "claude-haiku-4-5-20251001",
            "modelUsage": {
                "claude-sonnet-4-6": { "contextWindow": 1_000_000 },
                "claude-haiku-4-5-20251001": { "contextWindow": 200_000 }
            }
        });
        let (inner, _rx) = make_test_inner();
        let mut segment = SegmentState::default();
        let base_id = "test-msg".to_string();
        let mut current_id = base_id.clone();
        consume_claude_stream_value(
            &result_event,
            &mut summary,
            &mut segment,
            &inner,
            &base_id,
            &mut current_id,
        );

        assert_eq!(summary.result_context_window, Some(200_000));
    }

    #[test]
    fn estimate_context_breakdown_supports_explicit_1m_model_suffix_fallback() {
        let usage = json!({
            "input_tokens": 20,
            "output_tokens": 10,
            "total_tokens": 30,
            "cached_prompt_tokens": 0,
            "cache_creation_input_tokens": 0,
            "reasoning_tokens": 0
        });
        let bd = estimate_context_breakdown(Some(&usage), 0, 0, 0, None, Some("sonnet[1m]"));
        assert_eq!(
            bd.get("context_window").and_then(Value::as_u64),
            Some(CLAUDE_ESTIMATED_CONTEXT_WINDOW_1M)
        );
    }

    #[test]
    fn phase_usage_returns_raw_usage() {
        // phase_usage_for_emission returns summary.usage as-is (no
        // differential math) so the token badge and context breakdown
        // reflect what the latest API call actually consumed.
        let mut summary = ClaudeStdoutSummary::default();

        // No usage set → None.
        assert!(phase_usage_for_emission(&mut summary).is_none());

        summary.usage = Some(json!({
            "input_tokens": 150_000,
            "output_tokens": 500,
            "total_tokens": 150_500,
            "cached_prompt_tokens": 120_000,
            "cache_creation_input_tokens": 0,
            "reasoning_tokens": 0
        }));

        let phase1 = phase_usage_for_emission(&mut summary).expect("should return usage");
        assert_eq!(
            phase1.get("input_tokens").and_then(Value::as_u64),
            Some(150_000)
        );
        assert_eq!(
            phase1.get("output_tokens").and_then(Value::as_u64),
            Some(500)
        );
        assert_eq!(
            phase1.get("cached_prompt_tokens").and_then(Value::as_u64),
            Some(120_000)
        );
    }

    #[test]
    fn todo_write_emits_task_update() {
        let arguments = json!({
            "todos": [
                {"content": "Fix the bug", "status": "completed", "activeForm": "Fixing the bug"},
                {"content": "Run tests", "status": "in_progress", "activeForm": "Running tests"},
                {"content": "Deploy", "status": "pending", "activeForm": "Deploying"},
            ]
        });
        let event = claude_task_update_from_todo_write(&arguments)
            .expect("should produce a TaskUpdate event");
        assert_eq!(
            event.get("kind").and_then(Value::as_str),
            Some("TaskUpdate")
        );
        let data = event.get("data").expect("should have data");
        let tasks = data
            .get("tasks")
            .and_then(Value::as_array)
            .expect("should have tasks");
        assert_eq!(tasks.len(), 3);

        // Completed task uses `content` (imperative form).
        assert_eq!(
            tasks[0].get("description").and_then(Value::as_str),
            Some("Fix the bug")
        );
        assert_eq!(
            tasks[0].get("status").and_then(Value::as_str),
            Some("completed")
        );

        // In-progress task uses `activeForm` (present-tense).
        assert_eq!(
            tasks[1].get("description").and_then(Value::as_str),
            Some("Running tests")
        );
        assert_eq!(
            tasks[1].get("status").and_then(Value::as_str),
            Some("in_progress")
        );

        // Pending task uses `content`.
        assert_eq!(
            tasks[2].get("description").and_then(Value::as_str),
            Some("Deploy")
        );
        assert_eq!(
            tasks[2].get("status").and_then(Value::as_str),
            Some("pending")
        );
    }

    #[test]
    fn todo_write_returns_none_for_missing_todos() {
        assert!(claude_task_update_from_todo_write(&json!({})).is_none());
        assert!(claude_task_update_from_todo_write(&json!({"todos": "not an array"})).is_none());
    }

    #[test]
    fn extract_spawn_description_prefers_prompt_over_description() {
        let input = json!({
            "description": "Spawn test sub-agent",
            "prompt": "Say \"hello world\" and nothing else."
        });
        assert_eq!(
            extract_spawn_description(Some(&input)),
            "Say \"hello world\" and nothing else."
        );
    }

    #[tokio::test]
    async fn pending_subagent_prompt_is_emitted_on_content_block_stop() {
        let (relay_event_tx, mut relay_event_rx) = mpsc::unbounded_channel();
        let (raw_event_tx, raw_event_rx) = mpsc::unbounded_channel();
        spawn_claude_subagent_event_bridge(raw_event_rx, relay_event_tx);
        let mut streams = HashMap::new();
        streams.insert(
            "toolu_spawn".to_string(),
            SubAgentStream {
                summary: ClaudeStdoutSummary::default(),
                segment: SegmentState::default(),
                message_id: "subagent-toolu_spawn".to_string(),
                has_explicit_task_prompt: false,
                inner: Arc::new(ClaudeInner {
                    event_tx: raw_event_tx,
                    state: Mutex::new(ClaudeState::default()),
                }),
            },
        );

        let mut pending_prompts = HashMap::new();
        pending_prompts.insert(
            0,
            PendingSubAgentPrompt {
                tool_use_id: "toolu_spawn".to_string(),
                partial_json: "{\"prompt\":\"Say \\\"hello world\\\" and nothing else.\"}"
                    .to_string(),
            },
        );

        let stop_event = json!({
            "type": "stream_event",
            "event": {
                "type": "content_block_stop",
                "index": 0
            }
        });

        track_pending_subagent_prompt_event(&stop_event, &mut streams, &mut pending_prompts);

        let emitted = relay_event_rx
            .recv()
            .await
            .expect("prompt message should be emitted");
        let protocol::ChatEvent::MessageAdded(message) = emitted else {
            panic!("expected MessageAdded chat event");
        };
        assert_eq!(message.content, "Say \"hello world\" and nothing else.");
        assert!(pending_prompts.is_empty());
    }

    #[tokio::test]
    async fn ask_user_question_tool_emits_request_and_success_completion() {
        let (inner, mut rx) = make_test_inner();
        let mut summary = ClaudeStdoutSummary::default();
        let mut segment = SegmentState::default();
        let base_id = "claude-msg-1".to_string();
        let mut current_id = base_id.clone();

        // Outer code emits initial StreamStart before read_claude_stdout runs.
        inner.emit_stream_start(&base_id, None);
        let ev = rx.recv().await.unwrap();
        assert_eq!(event_kind(&ev), Some("StreamStart"));

        // 1) assistant message with AskUserQuestion tool_use
        consume_claude_stream_value(
            &json!({
                "type": "assistant",
                "message": {
                    "model": "claude-opus-4-6",
                    "id": "msg_ask",
                    "type": "message",
                    "role": "assistant",
                    "content": [{
                        "type": "tool_use",
                        "id": "toolu_ask",
                        "name": "AskUserQuestion",
                        "input": {
                            "questions": [{
                                "question": "Which language?",
                                "header": "Language",
                                "options": [
                                    { "label": "Rust", "description": "Systems lang" },
                                    { "label": "Python", "description": "Scripting lang" }
                                ],
                                "multiSelect": false
                            }]
                        }
                    }],
                    "usage": { "input_tokens": 100, "output_tokens": 50 }
                },
                "session_id": "test-session"
            }),
            &mut summary,
            &mut segment,
            &inner,
            &base_id,
            &mut current_id,
        );

        // At this point the tool_call is registered but no StreamEnd/ToolRequest emitted yet
        // (close_current_phase hasn't been called yet).
        assert!(
            summary.tool_name_by_id.contains_key("toolu_ask"),
            "tool call should be registered: {:?}",
            summary.tool_name_by_id
        );

        // 2) user message with tool_result (is_error: true) — this triggers
        //    close_current_phase (StreamEnd + ToolRequest) then ToolExecutionCompleted.
        consume_claude_stream_value(
            &json!({
                "type": "user",
                "message": {
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "content": "Answer questions?",
                        "is_error": true,
                        "tool_use_id": "toolu_ask"
                    }]
                },
                "session_id": "test-session"
            }),
            &mut summary,
            &mut segment,
            &inner,
            &base_id,
            &mut current_id,
        );

        // Drain all emitted events
        let mut events = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            events.push(ev);
        }

        let kinds: Vec<_> = events
            .iter()
            .filter_map(|ev| ev.get("kind").and_then(Value::as_str))
            .collect();

        // Expect: StreamEnd, ToolRequest, ToolExecutionCompleted
        assert!(
            kinds.contains(&"StreamEnd"),
            "expected StreamEnd in events, got: {kinds:?}"
        );
        assert!(
            kinds.contains(&"ToolRequest"),
            "expected ToolRequest in events, got: {kinds:?}"
        );
        assert!(
            kinds.contains(&"ToolExecutionCompleted"),
            "expected ToolExecutionCompleted in events, got: {kinds:?}"
        );

        // ToolExecutionCompleted should be success (overridden from is_error)
        let completion = events
            .iter()
            .find(|ev| event_kind(ev) == Some("ToolExecutionCompleted"))
            .expect("ToolExecutionCompleted should be present");
        assert_eq!(
            completion.pointer("/data/success").and_then(Value::as_bool),
            Some(true),
            "user-input tool should be overridden to success"
        );
        assert_eq!(
            completion
                .pointer("/data/tool_name")
                .and_then(Value::as_str),
            Some("AskUserQuestion")
        );
    }

    #[tokio::test]
    async fn exit_plan_mode_includes_plan_content_from_preceding_write() {
        let (inner, mut rx) = make_test_inner();
        let mut summary = ClaudeStdoutSummary::default();
        let mut segment = SegmentState::default();
        let base_id = "claude-msg-1".to_string();
        let mut current_id = base_id.clone();

        inner.emit_stream_start(&base_id, None);
        let ev = rx.recv().await.unwrap();
        assert_eq!(event_kind(&ev), Some("StreamStart"));

        let plan_content = "# Plan\n\n## Step 1\nDo the first thing.";

        // 1) assistant message with Write tool_use (plan file) + ExitPlanMode tool_use
        consume_claude_stream_value(
            &json!({
                "type": "assistant",
                "message": {
                    "model": "claude-opus-4-6",
                    "id": "msg_plan",
                    "type": "message",
                    "role": "assistant",
                    "content": [
                        {
                            "type": "tool_use",
                            "id": "toolu_write",
                            "name": "Write",
                            "input": {
                                "file_path": "/Users/test/.claude/plans/test-plan.md",
                                "content": plan_content
                            }
                        },
                        {
                            "type": "tool_use",
                            "id": "toolu_exit",
                            "name": "ExitPlanMode",
                            "input": {}
                        }
                    ],
                    "usage": { "input_tokens": 100, "output_tokens": 50 }
                },
                "session_id": "test-session"
            }),
            &mut summary,
            &mut segment,
            &inner,
            &base_id,
            &mut current_id,
        );

        // 2) user message with tool_results for both tools
        consume_claude_stream_value(
            &json!({
                "type": "user",
                "message": {
                    "role": "user",
                    "content": [
                        {
                            "type": "tool_result",
                            "content": "",
                            "is_error": false,
                            "tool_use_id": "toolu_write"
                        },
                        {
                            "type": "tool_result",
                            "content": "Exit plan mode?",
                            "is_error": true,
                            "tool_use_id": "toolu_exit"
                        }
                    ]
                },
                "session_id": "test-session"
            }),
            &mut summary,
            &mut segment,
            &inner,
            &base_id,
            &mut current_id,
        );

        // Drain all emitted events
        let mut events = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            events.push(ev);
        }

        // Find the ExitPlanMode ToolExecutionCompleted
        let completion = events
            .iter()
            .find(|ev| {
                event_kind(ev) == Some("ToolExecutionCompleted")
                    && ev.pointer("/data/tool_name").and_then(Value::as_str) == Some("ExitPlanMode")
            })
            .expect("ExitPlanMode ToolExecutionCompleted should be present");

        assert_eq!(
            completion.pointer("/data/success").and_then(Value::as_bool),
            Some(true),
        );

        // The result should contain the plan content from the Write tool
        assert_eq!(
            completion
                .pointer("/data/tool_result/result/plan_content")
                .and_then(Value::as_str),
            Some(plan_content),
            "ExitPlanMode result should include plan_content from preceding Write"
        );
    }

    #[tokio::test]
    async fn forward_claude_backend_event_fails_ready_on_pre_session_error() {
        let (ready_tx, ready_rx) = oneshot::channel::<Result<(), String>>();
        let ready_tx: ClaudeReadyTx = Arc::new(Mutex::new(Some(ready_tx)));
        let (events_tx, mut events_rx) = mpsc::channel::<ChatEvent>(4);
        let session_id = Arc::new(std::sync::Mutex::new(None));

        let forwarded = forward_claude_backend_event(
            json!({
                "kind": "Error",
                "data": "Failed to start Claude CLI: No such file or directory"
            }),
            &events_tx,
            &session_id,
            Some(&ready_tx),
        )
        .await;

        assert!(forwarded, "expected backend error event to be forwarded");
        assert_eq!(
            ready_rx.await.expect("ready result"),
            Err("Failed to start Claude CLI: No such file or directory".to_string())
        );

        let event = events_rx.recv().await.expect("forwarded chat event");
        match event {
            ChatEvent::MessageAdded(message) => {
                assert!(matches!(message.sender, protocol::MessageSender::Error));
                assert_eq!(
                    message.content,
                    "Failed to start Claude CLI: No such file or directory"
                );
            }
            other => panic!("expected error chat event, got {other:?}"),
        }
    }
}
