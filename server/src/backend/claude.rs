use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::DateTime;
use command_group::{AsyncCommandGroup, AsyncGroupChild};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::fs as tokio_fs;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::{Mutex, mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use protocol::{
    BackendAccessMode, CapacityBucket, CapacityBucketId, CapacityBucketStatus, CapacityCoverage,
    CapacityMeasure, CapacityReport, CapacityReset, CapacityScope, CapacitySource,
    CapacityUnavailableReason, CapacityWindow, ClaudeLimitType, ExitPlanModeDecision,
    SendMessageToolResponse, SessionId, ToolPolicy, ToolProgressData, ToolProgressUpdate,
    ValueProvenance, WorkflowAgentState, WorkflowAgentStatus, WorkflowRunState, WorkflowRunStatus,
};

use crate::backend::agent_control_progress::{
    await_progress_data_for_tool, spawn_progress_data_for_tool_result, tyde_tool_request_type,
    tyde_tool_result,
};
use crate::backend::turn_emitter::{
    AgentName, AssistantMessagePayload, StreamEndPayload, ToolCompletedPayload, TurnEmitter,
};
use crate::backend::{
    AgentIdentity, READ_ONLY_ACCESS_MODE_INSTRUCTIONS, SessionCommand, StartupMcpServer,
    StartupMcpTransport,
};
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
    /// The parent's Task tool_use id — the `tool_call_id` for live
    /// `ToolProgress` updates on the parent's Task tool card.
    parent_tool_use_id: String,
    /// Id of the spawned sub-agent (from `SubAgentHandle`), included in
    /// progress updates so the frontend can link to the sub-agent view.
    agent_id: protocol::AgentId,
    agent_name: String,
    /// Emitter of the PARENT agent, used for the progress updates above.
    parent_emitter: Arc<TurnEmitter>,
    last_progress_emit: std::time::Instant,
    /// How the CLI classified this sub-agent's execution lifecycle.
    /// Background agents keep streaming their own output *after* the parent
    /// receives the synthetic "launched" tool_result, so their stream must
    /// be finalized on the `task_notification` completion frame rather than
    /// torn down early when that placeholder tool_result arrives.
    execution: SubAgentExecution,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum SubAgentExecution {
    #[default]
    Unknown,
    Foreground,
    Background,
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
// Claude plan mode blocks build/test Bash; ReadOnly is advisory in Tyde.
const CLAUDE_READ_ONLY_PERMISSION_MODE: &str = "acceptEdits";
const CLAUDE_CONVERSATION_COMPACTED_NOTICE: &str = "Conversation compacted.";
const CLAUDE_INITIALIZE_TIMEOUT: Duration = Duration::from_secs(30);

#[cfg(test)]
pub(crate) static FAKE_CLAUDE_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
#[cfg(test)]
static CLAUDE_PROCESS_SPAWN_OBSERVER: std::sync::Mutex<Option<oneshot::Sender<u32>>> =
    std::sync::Mutex::new(None);
const CLAUDE_CONTROL_RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);
const CLAUDE_INTERRUPT_QUIESCE_TIMEOUT: Duration = Duration::from_secs(18);
const TYDE_CLAUDE_BIN_ENV: &str = "TYDE_CLAUDE_BIN";

#[cfg(test)]
fn observe_claude_process_spawned(pid: Option<u32>) {
    let Some(pid) = pid else {
        return;
    };
    if let Some(observer) = CLAUDE_PROCESS_SPAWN_OBSERVER
        .lock()
        .expect("Claude process spawn observer mutex poisoned")
        .take()
    {
        let _ = observer.send(pid);
    }
}

static CLAUDE_TURN_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Clone)]
pub struct ClaudeCommandHandle {
    inner: Arc<ClaudeInner>,
}

impl ClaudeCommandHandle {
    pub async fn execute(&self, command: SessionCommand) -> Result<(), String> {
        ClaudeInner::execute_arc(Arc::clone(&self.inner), command).await
    }

    async fn send_message_payload(
        &self,
        payload: protocol::SendMessagePayload,
    ) -> Result<(), String> {
        ClaudeInner::send_message(
            Arc::clone(&self.inner),
            payload.message,
            protocol_images_to_attachments(payload.images),
            payload.tool_response,
        )
        .await
    }
}

#[derive(Clone)]
pub struct ClaudeSession {
    inner: Arc<ClaudeInner>,
}

struct ClaudeSpawnMode<'a> {
    no_session_persistence: bool,
    fork_from_session_id: Option<String>,
    ssh_host: Option<String>,
    startup_mcp_servers: &'a [StartupMcpServer],
    steering_content: Option<&'a str>,
    agent_identity: Option<&'a AgentIdentity>,
    tool_policy: ToolPolicy,
    access_mode: BackendAccessMode,
}

struct ClaudeForkConfig<'a> {
    from_session_id: &'a str,
    ssh_host: Option<String>,
    startup_mcp_servers: &'a [StartupMcpServer],
    steering_content: Option<&'a str>,
    agent_identity: Option<&'a AgentIdentity>,
    tool_policy: ToolPolicy,
    access_mode: BackendAccessMode,
}

impl ClaudeSession {
    pub async fn spawn(
        workspace_roots: &[String],
        ssh_host: Option<String>,
        startup_mcp_servers: &[StartupMcpServer],
        steering_content: Option<&str>,
        agent_identity: Option<&AgentIdentity>,
        tool_policy: ToolPolicy,
        access_mode: BackendAccessMode,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Value>), String> {
        Self::spawn_with_mode(
            workspace_roots,
            ClaudeSpawnMode {
                no_session_persistence: false,
                fork_from_session_id: None,
                ssh_host,
                startup_mcp_servers,
                steering_content,
                agent_identity,
                tool_policy,
                access_mode,
            },
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
        access_mode: BackendAccessMode,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Value>), String> {
        Self::spawn_with_mode(
            workspace_roots,
            ClaudeSpawnMode {
                no_session_persistence: true,
                fork_from_session_id: None,
                ssh_host,
                startup_mcp_servers,
                steering_content,
                agent_identity,
                tool_policy,
                access_mode,
            },
        )
        .await
    }

    async fn fork(
        workspace_roots: &[String],
        fork_config: ClaudeForkConfig<'_>,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Value>), String> {
        let from_session_id = normalize_nonempty(fork_config.from_session_id)
            .ok_or_else(|| "Claude fork requires non-empty from_session_id".to_string())?;
        Self::spawn_with_mode(
            workspace_roots,
            ClaudeSpawnMode {
                no_session_persistence: false,
                fork_from_session_id: Some(from_session_id),
                ssh_host: fork_config.ssh_host,
                startup_mcp_servers: fork_config.startup_mcp_servers,
                steering_content: fork_config.steering_content,
                agent_identity: fork_config.agent_identity,
                tool_policy: fork_config.tool_policy,
                access_mode: fork_config.access_mode,
            },
        )
        .await
    }

    async fn spawn_with_mode(
        workspace_roots: &[String],
        mode: ClaudeSpawnMode<'_>,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Value>), String> {
        let (workspace_root, resolved_ssh_host) = if let Some(host) = mode.ssh_host {
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
            emitter: Arc::new(TurnEmitter::new_for_agent(
                event_tx,
                AgentName(CLAUDE_AGENT_NAME),
            )),
            state: Mutex::new(ClaudeState {
                workspace_root,
                ssh_host: resolved_ssh_host,
                session_id: None,
                fork_from_session_id: mode.fork_from_session_id,
                start_session_fresh: false,
                ephemeral: mode.no_session_persistence,
                model: None,
                effort: Some(ClaudeEffort::High),
                permission_mode: Some(
                    claude_permission_mode_for_access_mode(mode.access_mode).to_string(),
                ),
                startup_mcp_config_json: build_claude_mcp_config_json(mode.startup_mcp_servers),
                steering_content: mode.steering_content.map(|s| s.to_string()),
                agent_identity: mode.agent_identity.cloned(),
                tool_policy: mode.tool_policy,
                cumulative_usage: None,
                cumulative_usage_complete: true,
                conversation_bytes_total: 0,
                active_turn: None,
                restart_process_after_turn: false,
                subagent_emitter: None,
                capacity_access: ClaudeCapacityAccess::Unknown,
                capacity_refresh_in_flight: false,
                capacity_report_emitted: false,
                authoritative_capacity_emitted: false,
            }),
            runtime: Mutex::new(None),
            turn_event_gate: Mutex::new(()),
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
    outcome_tx: Option<oneshot::Sender<TurnOutcome>>,
    interrupt_requested: bool,
    pending_ask_user_question: Option<PendingAskUserQuestionControl>,
    pending_exit_plan_mode: Option<PendingExitPlanModeControl>,
    quiesced_waiters: Vec<oneshot::Sender<()>>,
}

#[derive(Clone)]
struct PendingAskUserQuestionControl {
    request_id: String,
    tool_call_id: String,
    tool_name: String,
    input: Value,
}

#[derive(Clone)]
struct PendingExitPlanModeControl {
    request_id: String,
    tool_call_id: String,
    tool_name: String,
    input: Value,
    plan: Option<String>,
    plan_path: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClaudeEffort {
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

impl ClaudeEffort {
    const ALL: [Self; 5] = [Self::Low, Self::Medium, Self::High, Self::XHigh, Self::Max];

    fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Low => "Low",
            Self::Medium => "Medium",
            Self::High => "High",
            Self::XHigh => "XHigh",
            Self::Max => "Max",
        }
    }

    fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            "xhigh" => Ok(Self::XHigh),
            "max" => Ok(Self::Max),
            value => Err(format!(
                "unsupported Claude effort '{value}'; expected one of: {}",
                Self::ALL
                    .iter()
                    .map(|effort| effort.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }
}

struct ClaudeState {
    workspace_root: String,
    ssh_host: Option<String>,
    session_id: Option<String>,
    fork_from_session_id: Option<String>,
    start_session_fresh: bool,
    ephemeral: bool,
    model: Option<String>,
    effort: Option<ClaudeEffort>,
    permission_mode: Option<String>,
    startup_mcp_config_json: Option<String>,
    steering_content: Option<String>,
    agent_identity: Option<AgentIdentity>,
    tool_policy: ToolPolicy,
    cumulative_usage: Option<Value>,
    cumulative_usage_complete: bool,
    conversation_bytes_total: u64,
    active_turn: Option<ActiveTurn>,
    restart_process_after_turn: bool,
    subagent_emitter: Option<Arc<dyn SubAgentEmitter>>,
    capacity_access: ClaudeCapacityAccess,
    capacity_refresh_in_flight: bool,
    capacity_report_emitted: bool,
    authoritative_capacity_emitted: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ClaudeCapacityAccess {
    #[default]
    Unknown,
    Subscription,
    ApiKey,
    ExternalProvider,
}

impl Default for ClaudeState {
    fn default() -> Self {
        Self {
            workspace_root: String::new(),
            ssh_host: None,
            session_id: None,
            fork_from_session_id: None,
            start_session_fresh: false,
            ephemeral: false,
            model: None,
            effort: None,
            permission_mode: None,
            startup_mcp_config_json: None,
            steering_content: None,
            agent_identity: None,
            tool_policy: ToolPolicy::Unrestricted,
            cumulative_usage: None,
            cumulative_usage_complete: true,
            conversation_bytes_total: 0,
            active_turn: None,
            restart_process_after_turn: false,
            subagent_emitter: None,
            capacity_access: ClaudeCapacityAccess::Unknown,
            capacity_refresh_in_flight: false,
            capacity_report_emitted: false,
            authoritative_capacity_emitted: false,
        }
    }
}

struct ClaudeInner {
    /// Typed emitter enforcing protocol ordering (stream pairing, tool
    /// pairing, cancellation sequence). Every wire event — including
    /// session-control ones like `SessionStarted` / `Error` — goes
    /// through here; there is no raw `event_tx` fallback.
    emitter: Arc<TurnEmitter>,
    state: Mutex<ClaudeState>,
    runtime: Mutex<Option<ClaudeProcessRuntime>>,
    turn_event_gate: Mutex<()>,
}

struct ClaudeProcessRuntime {
    stdin: Arc<Mutex<ChildStdin>>,
    child: Arc<Mutex<Option<AsyncGroupChild>>>,
    control_waiters: ClaudeControlWaiters,
    stdout_task: JoinHandle<()>,
    stderr_task: JoinHandle<()>,
}

type ClaudeControlWaiters = Arc<Mutex<HashMap<String, oneshot::Sender<Result<Value, String>>>>>;

impl ClaudeProcessRuntime {
    async fn kill(self) {
        self.stdout_task.abort();
        self.stderr_task.abort();
        let mut child = self.child.lock().await;
        if let Some(child) = child.as_mut() {
            let _ = child.kill().await;
        }
        *child = None;
    }
}

impl Drop for ClaudeProcessRuntime {
    /// Reaps the child and aborts the reader tasks. This Drop genuinely fires
    /// on both real leak paths:
    ///
    /// - Process self-exit (the dominant leak): the stdout reader hits EOF and
    ///   calls `mark_process_exited`, which `take()`s the runtime out of its
    ///   slot in `ClaudeInner`; the taken runtime then drops here. (It does NOT
    ///   wait for the `Arc<ClaudeInner>` cycle to resolve, so it fires promptly
    ///   even while other tasks still hold `ClaudeInner`.) The detached reaper
    ///   `wait()`s the child, fixing the case where the old bare `try_wait`
    ///   raced the not-yet-reaped child and left a zombie.
    /// - Client disconnect / teardown: `shutdown()` → `shutdown_process()` →
    ///   `kill()` reaps the still-running child first; Drop is then a no-op
    ///   (child already `None`).
    ///
    /// So Drop is a real reaper on exit and a last-ditch net otherwise.
    fn drop(&mut self) {
        self.stdout_task.abort();
        self.stderr_task.abort();
        crate::backend::subprocess::reap_group_child_slot(&self.child);
    }
}

struct ClaudeResumeStartupGuard {
    session: Option<ClaudeSession>,
}

struct ClaudeDetachedStartupCancelGuard(Option<oneshot::Sender<()>>);

impl ClaudeDetachedStartupCancelGuard {
    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for ClaudeDetachedStartupCancelGuard {
    fn drop(&mut self) {
        if let Some(cancel) = self.0.take() {
            let _ = cancel.send(());
        }
    }
}

impl ClaudeResumeStartupGuard {
    fn new(session: ClaudeSession) -> Self {
        Self {
            session: Some(session),
        }
    }

    fn disarm(&mut self) {
        self.session = None;
    }
}

impl Drop for ClaudeResumeStartupGuard {
    fn drop(&mut self) {
        let Some(session) = self.session.take() else {
            return;
        };
        tokio::spawn(async move {
            session.shutdown().await;
        });
    }
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
    /// Aggregate usage for this CLI invocation from the `result` event.
    /// Kept separate from `usage` so we don't confuse a turn with one API call.
    result_turn_usage: Option<Value>,
    /// Sum of distinct API-call usages observed while relaying a native child.
    /// Claude does not consistently correlate a `result` frame to native children.
    accumulated_request_usage: Option<Value>,
    /// Context window extracted from `result.modelUsage[model].contextWindow`.
    result_context_window: Option<u64>,
    errors: Vec<String>,
    tool_calls: Vec<ClaudeToolCall>,
    seen_tool_ids: HashSet<String>,
    tool_name_by_id: HashMap<String, String>,
    tool_call_by_id: HashMap<String, ClaudeToolCall>,
    tool_modify_preview_by_id: HashMap<String, ClaudeModifyPreview>,
    unresolved_tool_requests: HashMap<String, String>,
    auto_closed_tool_requests: HashSet<String>,
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
    #[serde(default)]
    workflow_name: Option<String>,
    /// Aggregate usage on `task_progress` frames.
    #[serde(default)]
    usage: Option<ClaudeTaskUsage>,
    /// Per-workflow-agent delta events on `task_progress` frames. Each
    /// entry is parsed individually into `ClaudeWorkflowAgentDelta` so
    /// one malformed delta is surfaced and skipped without losing the
    /// rest of the frame.
    #[serde(default)]
    workflow_progress: Option<Vec<Value>>,
}

#[derive(Debug, Deserialize)]
struct ClaudeTaskUsage {
    #[serde(default)]
    total_tokens: Option<u64>,
    #[serde(default)]
    tool_uses: Option<u64>,
    #[serde(default)]
    duration_ms: Option<u64>,
}

/// One entry of a `task_progress` frame's `workflow_progress` array.
/// `kind` stays a string at this boundary: the array carries entry
/// types beyond `workflow_agent` (e.g. workflow-level records) that
/// this reducer intentionally ignores, and the set is owned by the CLI,
/// not by Tyde. Everything consumed from it maps into typed protocol
/// state.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaudeWorkflowAgentDelta {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    index: Option<u64>,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    phase_title: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    attempt: Option<u64>,
    #[serde(default)]
    tokens: Option<u64>,
    #[serde(default)]
    tool_calls: Option<u64>,
    #[serde(default)]
    duration_ms: Option<u64>,
    #[serde(default)]
    prompt_preview: Option<String>,
    #[serde(default)]
    result_preview: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ClaudeSystemEvent {
    Init,
    Status,
    CompactBoundary,
    TaskStarted,
    TaskProgress,
    TaskNotification,
    BackgroundTasksChanged,
    TaskUpdated,
    ThinkingTokens,
    ApiRetry,
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
            "background_tasks_changed" => ClaudeSystemEvent::BackgroundTasksChanged,
            "task_updated" => ClaudeSystemEvent::TaskUpdated,
            "thinking_tokens" => ClaudeSystemEvent::ThinkingTokens,
            "api_retry" => ClaudeSystemEvent::ApiRetry,
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

#[derive(Debug, Clone)]
struct ClaudeTurnUsage {
    turn: Value,
    cumulative: Option<Value>,
}

#[derive(Debug, Clone, Default)]
struct ClaudeMessageUsage {
    request: Option<Value>,
    turn: Option<Value>,
    cumulative: Option<Value>,
}

enum ClaudeHistoryReplayItem {
    Message(Value),
    ToolRequest(ClaudeToolCall),
    ToolExecutionCompleted(ClaudeReplayToolExecution),
}

struct ClaudeSessionReplay {
    items: Vec<ClaudeHistoryReplayItem>,
    cumulative_usage: Option<Value>,
    cumulative_usage_complete: bool,
    conversation_bytes_total: u64,
}

#[derive(Debug)]
enum ClaudeSessionHistoryError {
    Missing { target: String, detail: String },
    Other(String),
}

impl ClaudeSessionHistoryError {
    fn missing(target: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::Missing {
            target: target.into(),
            detail: detail.into(),
        }
    }

    fn other(message: impl Into<String>) -> Self {
        Self::Other(message.into())
    }

    fn is_missing(&self) -> bool {
        matches!(self, Self::Missing { .. })
    }
}

impl std::fmt::Display for ClaudeSessionHistoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing { target, detail } => {
                write!(f, "Claude session history '{target}' is missing: {detail}")
            }
            Self::Other(message) => f.write_str(message),
        }
    }
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

impl TurnOutcome {
    fn summary(&self) -> &ClaudeStdoutSummary {
        match self {
            TurnOutcome::Completed { summary, .. } => summary,
            TurnOutcome::Cancelled { summary } => summary,
            TurnOutcome::Failed { summary, .. } => summary,
        }
    }
}

#[cfg(test)]
struct RunTurnParams<'a> {
    message_id: &'a str,
    workspace_root: &'a str,
    ssh_host: Option<&'a str>,
    prompt: &'a str,
    images: &'a [ImageAttachment],
    session_id: Option<String>,
    ephemeral: bool,
    model: Option<String>,
    effort: Option<ClaudeEffort>,
    permission_mode: Option<String>,
    startup_mcp_config_json: Option<String>,
    steering_content: Option<String>,
    agent_identity: Option<AgentIdentity>,
    tool_policy: ToolPolicy,
}

enum TurnStartError {
    Cancelled,
    Failed(String),
}

struct ClaudeProcessSpawnConfig {
    workspace_root: String,
    ssh_host: Option<String>,
    session_id: Option<String>,
    fork_from_session_id: Option<String>,
    resume_existing_session: bool,
    ephemeral: bool,
    model: Option<String>,
    effort: Option<ClaudeEffort>,
    permission_mode: Option<String>,
    startup_mcp_config_json: Option<String>,
    steering_content: Option<String>,
    agent_identity: Option<AgentIdentity>,
    tool_policy: ToolPolicy,
}

#[derive(Clone)]
struct AskUserQuestionControlRequest {
    request_id: String,
    tool_call_id: String,
    tool_name: String,
    input: Value,
}

#[derive(Clone)]
struct ExitPlanModeControlRequest {
    request_id: String,
    tool_call_id: String,
    tool_name: String,
    input: Value,
}

#[cfg(test)]
#[derive(Clone)]
struct AskAnswerRaceHook {
    after_write: Arc<tokio::sync::Notify>,
    resume: Arc<tokio::sync::Notify>,
}

#[cfg(test)]
static ASK_ANSWER_RACE_HOOK: std::sync::Mutex<Option<AskAnswerRaceHook>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
async fn pause_after_ask_answer_control_response_write_for_test() {
    let hook = ASK_ANSWER_RACE_HOOK
        .lock()
        .expect("AskUserQuestion answer race hook mutex poisoned")
        .clone();
    if let Some(hook) = hook {
        hook.after_write.notify_one();
        hook.resume.notified().await;
    }
}

impl ClaudeInner {
    async fn execute_arc(this: Arc<Self>, command: SessionCommand) -> Result<(), String> {
        match command {
            SessionCommand::SendMessage { message, images } => {
                Self::send_message(this, message, images, None).await
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
                this.emitter.profiles_list(Vec::new());
                Ok(())
            }
            SessionCommand::SwitchProfile { profile_name: _ } => Ok(()),
            SessionCommand::GetModuleSchemas => {
                this.emitter.module_schemas(Vec::new());
                Ok(())
            }
            SessionCommand::ListModels => {
                this.emitter.models_list(claude_known_models());
                Ok(())
            }
            SessionCommand::UpdateSettings {
                settings,
                persist: _,
            } => {
                let mut changed_process_setting = false;
                if let Some(obj) = settings.as_object() {
                    let effort_update = obj
                        .get("effort")
                        .or_else(|| obj.get("reasoning_effort"))
                        .map(parse_claude_effort_setting)
                        .transpose()?;
                    let mut state = this.state.lock().await;
                    if let Some(model_value) = obj.get("model") {
                        let next = normalize_optional_string(model_value);
                        changed_process_setting |= state.model != next;
                        state.model = next;
                    }

                    if let Some(next) = effort_update {
                        changed_process_setting |= state.effort != next;
                        state.effort = next;
                    }

                    if let Some(permission_mode_value) = obj
                        .get("permission_mode")
                        .or_else(|| obj.get("permissionMode"))
                        .or_else(|| obj.get("approval_policy"))
                    {
                        if permission_mode_value.is_null() {
                            changed_process_setting |= state.permission_mode.is_some();
                            state.permission_mode = None;
                        } else if let Some(permission_mode) =
                            normalize_claude_permission_mode(permission_mode_value)
                        {
                            changed_process_setting |=
                                state.permission_mode.as_deref() != Some(permission_mode.as_str());
                            state.permission_mode = Some(permission_mode);
                        }
                    }

                    if changed_process_setting {
                        state.restart_process_after_turn = state.active_turn.is_some();
                    }
                }
                if changed_process_setting {
                    let should_shutdown_now = {
                        let state = this.state.lock().await;
                        state.active_turn.is_none()
                    };
                    if should_shutdown_now {
                        this.shutdown_process().await;
                    }
                }
                this.emit_settings().await;
                Ok(())
            }
        }
    }

    async fn send_message(
        this: Arc<Self>,
        message: String,
        images: Option<Vec<ImageAttachment>>,
        tool_response: Option<SendMessageToolResponse>,
    ) -> Result<(), String> {
        if let Some(tool_response) = tool_response {
            if this
                .answer_pending_tool_response(tool_response, message.clone())
                .await?
            {
                return Ok(());
            }
            this.emit_error("No matching pending tool request is waiting for that response.");
            return Ok(());
        }

        if this
            .answer_pending_ask_user_question(message.clone(), images.clone())
            .await?
        {
            return Ok(());
        }
        this.emit_user_message_added(&message, images.as_deref());
        this.start_turn(message, images).await;
        Ok(())
    }

    async fn start_turn(self: Arc<Self>, message: String, images: Option<Vec<ImageAttachment>>) {
        let images = images.unwrap_or_default();
        let input_bytes = estimate_turn_input_bytes(&message, &images);
        let (turn_id, conversation_history_bytes, model_hint, ephemeral, outcome_rx) = {
            let mut state = self.state.lock().await;
            if state.active_turn.is_some() {
                self.emit_error("Claude is still processing the previous turn.");
                return;
            }

            let turn_id = CLAUDE_TURN_COUNTER.fetch_add(1, Ordering::Relaxed);
            let (outcome_tx, outcome_rx) = oneshot::channel();
            state.active_turn = Some(ActiveTurn {
                id: turn_id,
                outcome_tx: Some(outcome_tx),
                interrupt_requested: false,
                pending_ask_user_question: None,
                pending_exit_plan_mode: None,
                quiesced_waiters: Vec::new(),
            });
            state.conversation_bytes_total =
                state.conversation_bytes_total.saturating_add(input_bytes);

            (
                turn_id,
                state.conversation_bytes_total,
                state.model.clone(),
                state.ephemeral,
                outcome_rx,
            )
        };

        let message_id = format!("claude-msg-{turn_id}");
        self.emit_typing_status(true);
        self.emit_stream_start(&message_id, model_hint.clone());

        tokio::spawn(async move {
            match self
                .write_turn_to_persistent_process(turn_id, &message, &images)
                .await
            {
                Ok(()) => {}
                Err(TurnStartError::Cancelled) => {
                    self.complete_active_turn_with_outcome(
                        turn_id,
                        TurnOutcome::Cancelled {
                            summary: ClaudeStdoutSummary::default(),
                        },
                    )
                    .await;
                }
                Err(TurnStartError::Failed(error)) => {
                    self.complete_active_turn_with_outcome(
                        turn_id,
                        TurnOutcome::Failed {
                            summary: ClaudeStdoutSummary::default(),
                            error,
                        },
                    )
                    .await;
                }
            }

            let outcome = outcome_rx.await.unwrap_or_else(|_| TurnOutcome::Failed {
                summary: ClaudeStdoutSummary::default(),
                error: "Claude turn ended before returning a result".to_string(),
            });

            self.finalize_turn(
                turn_id,
                outcome,
                ephemeral,
                conversation_history_bytes,
                model_hint,
            )
            .await;
        });
    }

    #[cfg(test)]
    async fn run_turn(
        self: &Arc<Self>,
        params: RunTurnParams<'_>,
        _cancel_rx: oneshot::Receiver<()>,
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

        let turn_id = CLAUDE_TURN_COUNTER.fetch_add(1, Ordering::Relaxed);
        let (outcome_tx, outcome_rx) = oneshot::channel();
        {
            let mut state = self.state.lock().await;
            state.workspace_root = workspace_root.to_string();
            state.ssh_host = ssh_host.map(str::to_string);
            state.session_id = session_id;
            state.start_session_fresh = false;
            state.ephemeral = ephemeral;
            state.model = model.clone();
            state.effort = effort;
            state.permission_mode = permission_mode;
            state.startup_mcp_config_json = startup_mcp_config_json;
            state.steering_content = steering_content;
            state.agent_identity = agent_identity;
            state.tool_policy = tool_policy;
            state.active_turn = Some(ActiveTurn {
                id: turn_id,
                outcome_tx: Some(outcome_tx),
                interrupt_requested: false,
                pending_ask_user_question: None,
                pending_exit_plan_mode: None,
                quiesced_waiters: Vec::new(),
            });
            state.conversation_bytes_total = state
                .conversation_bytes_total
                .saturating_add(estimate_turn_input_bytes(prompt, images));
        }

        match self
            .write_turn_to_persistent_process(turn_id, prompt, images)
            .await
        {
            Ok(()) => {}
            Err(TurnStartError::Cancelled) => {
                self.complete_active_turn_with_outcome(
                    turn_id,
                    TurnOutcome::Cancelled {
                        summary: ClaudeStdoutSummary::default(),
                    },
                )
                .await;
            }
            Err(TurnStartError::Failed(error)) => {
                self.complete_active_turn_with_outcome(
                    turn_id,
                    TurnOutcome::Failed {
                        summary: ClaudeStdoutSummary::default(),
                        error,
                    },
                )
                .await;
            }
        }

        let outcome = outcome_rx.await.unwrap_or_else(|_| TurnOutcome::Failed {
            summary: ClaudeStdoutSummary::default(),
            error: "Claude turn ended before returning a result".to_string(),
        });
        if !ephemeral && let Some(session_id) = outcome.summary().session_id.clone() {
            self.set_session_id(session_id.clone()).await;
            self.emitter.session_started(&session_id);
        }
        let waiters = self.clear_active_turn(turn_id).await;
        notify_turn_quiesced(waiters);
        self.shutdown_process().await;
        let _ = message_id;
        outcome
    }

    async fn finalize_turn(
        self: &Arc<Self>,
        turn_id: u64,
        outcome: TurnOutcome,
        ephemeral: bool,
        conversation_history_bytes: u64,
        model_hint: Option<String>,
    ) {
        let pending_question_failure = match &outcome {
            TurnOutcome::Cancelled { .. } => Some("Claude turn cancelled.".to_string()),
            TurnOutcome::Failed { error, .. } => Some(error.clone()),
            TurnOutcome::Completed { .. } => None,
        };
        if let Some(message) = pending_question_failure.as_deref() {
            self.fail_pending_ask_user_question(turn_id, message).await;
            self.fail_pending_exit_plan_mode(turn_id, message).await;
        }

        // Persist the CLI-assigned session id regardless of turn outcome.
        // Claude writes its JSONL as events stream; our backend state must
        // track that id so any later process restart can `--resume` it.
        if !ephemeral && let Some(session_id) = outcome.summary().session_id.clone() {
            self.set_session_id(session_id.clone()).await;
            self.emitter.session_started(&session_id);
        }

        match outcome {
            TurnOutcome::Completed {
                summary,
                model_hint: result_model_hint,
            } => {
                let mut summary = summary;
                let turn_usage = self
                    .normalize_usage_for_turn(summary.result_turn_usage.clone())
                    .await;
                let known_context_window = summary.result_context_window;
                if !self
                    .emit_terminal_phase_or_placeholder(
                        &mut summary,
                        conversation_history_bytes,
                        known_context_window,
                        result_model_hint.or(model_hint),
                        turn_usage,
                    )
                    .await
                    && summary.emitted_phase_count == 0
                {
                    self.emit_error("Claude returned no assistant output.");
                }
            }
            TurnOutcome::Cancelled { summary } => {
                let mut summary = summary;
                let turn_usage = self
                    .normalize_usage_for_turn(summary.result_turn_usage.clone())
                    .await;
                let known_context_window = summary.result_context_window;
                self.emit_terminal_phase_or_placeholder(
                    &mut summary,
                    conversation_history_bytes,
                    known_context_window,
                    None,
                    turn_usage,
                )
                .await;
                let quiesced_waiters = self.clear_active_turn(turn_id).await;
                self.emit_operation_cancelled("Claude turn cancelled.");
                notify_turn_quiesced(quiesced_waiters);
                if self.take_restart_process_after_turn().await {
                    self.shutdown_process().await;
                }
                return;
            }
            TurnOutcome::Failed { summary, error } => {
                let mut summary = summary;
                let turn_usage = self
                    .normalize_usage_for_turn(summary.result_turn_usage.take())
                    .await;
                let known_context_window = summary.result_context_window;
                let _ = self
                    .emit_terminal_phase_or_placeholder(
                        &mut summary,
                        conversation_history_bytes,
                        known_context_window,
                        None,
                        turn_usage,
                    )
                    .await;
                let detail = summary.error_message().unwrap_or(error);
                self.emit_error(&detail);
            }
        }

        let quiesced_waiters = self.clear_active_turn(turn_id).await;
        self.emit_typing_status(false);
        notify_turn_quiesced(quiesced_waiters);
        if self.take_restart_process_after_turn().await {
            self.shutdown_process().await;
        }
    }

    /// Open a turn for output the Claude CLI produced on its own initiative,
    /// with no pending user message — e.g. when the model resumes after a
    /// background sub-agent finishes. Mirrors the scaffolding `start_turn`
    /// builds (allocate a turn id, emit typing + stream start, spawn the
    /// finalizer that awaits the outcome) so the unsolicited turn flows
    /// through the exact same completion path as a user-initiated one.
    /// Returns `None` if a turn is somehow already active.
    async fn begin_cli_initiated_turn(self: &Arc<Self>) -> Option<u64> {
        let (turn_id, ephemeral, conversation_history_bytes, model_hint, outcome_rx) = {
            let mut state = self.state.lock().await;
            if state.active_turn.is_some() {
                return None;
            }
            let turn_id = CLAUDE_TURN_COUNTER.fetch_add(1, Ordering::Relaxed);
            let (outcome_tx, outcome_rx) = oneshot::channel();
            state.active_turn = Some(ActiveTurn {
                id: turn_id,
                outcome_tx: Some(outcome_tx),
                interrupt_requested: false,
                pending_ask_user_question: None,
                pending_exit_plan_mode: None,
                quiesced_waiters: Vec::new(),
            });
            (
                turn_id,
                state.ephemeral,
                state.conversation_bytes_total,
                state.model.clone(),
                outcome_rx,
            )
        };

        let message_id = format!("claude-msg-{turn_id}");
        self.emit_typing_status(true);
        self.emit_stream_start(&message_id, model_hint.clone());

        let this = Arc::clone(self);
        tokio::spawn(async move {
            let outcome = outcome_rx.await.unwrap_or_else(|_| TurnOutcome::Failed {
                summary: ClaudeStdoutSummary::default(),
                error: "Claude turn ended before returning a result".to_string(),
            });
            this.finalize_turn(
                turn_id,
                outcome,
                ephemeral,
                conversation_history_bytes,
                model_hint,
            )
            .await;
        });

        Some(turn_id)
    }

    async fn write_turn_to_persistent_process(
        self: &Arc<Self>,
        turn_id: u64,
        prompt: &str,
        images: &[ImageAttachment],
    ) -> Result<(), TurnStartError> {
        self.ensure_process_ready()
            .await
            .map_err(TurnStartError::Failed)?;

        if self.active_turn_interrupted(turn_id).await {
            return Err(TurnStartError::Cancelled);
        }

        let input_message = build_stream_json_user_message(prompt, images);
        self.write_process_json_line(&input_message)
            .await
            .map_err(TurnStartError::Failed)
    }

    async fn begin_ask_user_question_control_request(
        &self,
        request: AskUserQuestionControlRequest,
    ) -> Result<(), String> {
        {
            let mut state = self.state.lock().await;
            let active = state
                .active_turn
                .as_mut()
                .ok_or_else(|| "Claude asked a question with no active turn".to_string())?;
            if active.pending_ask_user_question.is_some() {
                return Err(
                    "Claude asked a second question before the first was answered".to_string(),
                );
            }
            active.pending_ask_user_question = Some(PendingAskUserQuestionControl {
                request_id: request.request_id,
                tool_call_id: request.tool_call_id,
                tool_name: request.tool_name,
                input: request.input,
            });
        }

        self.emit_typing_status(false);
        Ok(())
    }

    async fn begin_exit_plan_mode_control_request(
        &self,
        request: ExitPlanModeControlRequest,
    ) -> Result<(), String> {
        let plan_info = exit_plan_mode_plan_info_from_arguments(&request.input);
        {
            let mut state = self.state.lock().await;
            let active = state
                .active_turn
                .as_mut()
                .ok_or_else(|| "Claude requested plan approval with no active turn".to_string())?;
            if active.pending_ask_user_question.is_some() || active.pending_exit_plan_mode.is_some()
            {
                return Err(
                    "Claude requested plan approval while another user response is pending"
                        .to_string(),
                );
            }
            active.pending_exit_plan_mode = Some(PendingExitPlanModeControl {
                request_id: request.request_id,
                tool_call_id: request.tool_call_id,
                tool_name: request.tool_name,
                input: request.input,
                plan: plan_info.plan,
                plan_path: plan_info.plan_path,
            });
        }

        self.emit_typing_status(false);
        Ok(())
    }

    async fn answer_pending_tool_response(
        &self,
        tool_response: SendMessageToolResponse,
        message: String,
    ) -> Result<bool, String> {
        match tool_response {
            SendMessageToolResponse::ExitPlanMode {
                tool_call_id,
                decision,
                feedback,
            } => {
                self.answer_pending_exit_plan_mode(tool_call_id, decision, feedback, message)
                    .await
            }
        }
    }

    async fn answer_pending_ask_user_question(
        &self,
        message: String,
        images: Option<Vec<ImageAttachment>>,
    ) -> Result<bool, String> {
        let _turn_event_guard = self.turn_event_gate.lock().await;
        let (turn_id, pending) = {
            let state = self.state.lock().await;
            let Some(active) = state.active_turn.as_ref() else {
                return Ok(false);
            };
            (active.id, active.pending_ask_user_question.clone())
        };
        let Some(pending) = pending else {
            return Ok(false);
        };

        let updated_input = ask_user_question_input_with_answer(&pending.input, &message);
        let payload =
            ask_user_question_control_response_payload(&pending.request_id, updated_input.clone());
        if let Err(err) = self.write_process_json_line(&payload).await {
            self.fail_pending_ask_user_question(
                turn_id,
                &format!("Failed to send AskUserQuestion answer to Claude: {err}"),
            )
            .await;
            self.complete_active_turn_with_outcome(
                turn_id,
                TurnOutcome::Failed {
                    summary: ClaudeStdoutSummary::default(),
                    error: format!("Failed to send AskUserQuestion answer to Claude: {err}"),
                },
            )
            .await;
            self.shutdown_process().await;
            return Ok(true);
        }

        #[cfg(test)]
        pause_after_ask_answer_control_response_write_for_test().await;

        let Some(pending) = self
            .take_pending_ask_user_question(turn_id, &pending.request_id)
            .await
        else {
            return Ok(true);
        };

        self.emit_user_message_added(&message, images.as_deref());
        self.emit_tool_execution_completed(
            &pending.tool_call_id,
            &pending.tool_name,
            true,
            json!({
                "kind": "Other",
                "result": {
                    "answer": message,
                    "updated_input": updated_input.clone(),
                },
            }),
            None,
        );
        self.emit_typing_status(true);
        Ok(true)
    }

    async fn answer_pending_exit_plan_mode(
        &self,
        tool_call_id: String,
        decision: ExitPlanModeDecision,
        feedback: Option<String>,
        message: String,
    ) -> Result<bool, String> {
        let _turn_event_guard = self.turn_event_gate.lock().await;
        let (turn_id, pending) = {
            let state = self.state.lock().await;
            let Some(active) = state.active_turn.as_ref() else {
                return Ok(false);
            };
            (active.id, active.pending_exit_plan_mode.clone())
        };
        let Some(pending) = pending else {
            return Ok(false);
        };
        if pending.tool_call_id != tool_call_id {
            self.emit_error(&format!(
                "ExitPlanMode response targeted stale tool_call_id {tool_call_id}; pending tool_call_id is {}.",
                pending.tool_call_id
            ));
            return Ok(true);
        }

        let normalized_feedback = feedback
            .and_then(|value| normalize_nonempty(&value))
            .or_else(|| normalize_nonempty(&message))
            .unwrap_or_else(|| "Plan rejected by user.".to_string());
        let payload = exit_plan_mode_control_response_payload(
            &pending.request_id,
            decision,
            pending.input.clone(),
            &normalized_feedback,
        );
        if let Err(err) = self.write_process_json_line(&payload).await {
            self.fail_pending_exit_plan_mode(
                turn_id,
                &format!("Failed to send ExitPlanMode response to Claude: {err}"),
            )
            .await;
            self.complete_active_turn_with_outcome(
                turn_id,
                TurnOutcome::Failed {
                    summary: ClaudeStdoutSummary::default(),
                    error: format!("Failed to send ExitPlanMode response to Claude: {err}"),
                },
            )
            .await;
            self.shutdown_process().await;
            return Ok(true);
        }

        let Some(pending) = self
            .take_pending_exit_plan_mode(turn_id, &pending.request_id)
            .await
        else {
            return Ok(true);
        };

        let decision_label = match decision {
            ExitPlanModeDecision::Approve => "approved",
            ExitPlanModeDecision::Reject => "rejected",
        };
        let mut result = serde_json::Map::new();
        result.insert(
            "decision".to_string(),
            Value::String(decision_label.to_string()),
        );
        if decision == ExitPlanModeDecision::Reject {
            result.insert(
                "feedback".to_string(),
                Value::String(normalized_feedback.clone()),
            );
        }
        if let Some(plan) = pending.plan {
            result.insert("plan".to_string(), Value::String(plan));
        }
        if let Some(plan_path) = pending.plan_path {
            result.insert("plan_path".to_string(), Value::String(plan_path));
        }

        self.emit_tool_execution_completed(
            &pending.tool_call_id,
            &pending.tool_name,
            true,
            json!({
                "kind": "Other",
                "result": Value::Object(result),
            }),
            None,
        );
        self.emit_typing_status(true);
        Ok(true)
    }

    async fn take_pending_ask_user_question(
        &self,
        turn_id: u64,
        request_id: &str,
    ) -> Option<PendingAskUserQuestionControl> {
        let mut state = self.state.lock().await;
        let active = state.active_turn.as_mut()?;
        if active.id != turn_id {
            return None;
        }
        if active
            .pending_ask_user_question
            .as_ref()
            .is_some_and(|pending| pending.request_id == request_id)
        {
            active.pending_ask_user_question.take()
        } else {
            None
        }
    }

    async fn take_pending_exit_plan_mode(
        &self,
        turn_id: u64,
        request_id: &str,
    ) -> Option<PendingExitPlanModeControl> {
        let mut state = self.state.lock().await;
        let active = state.active_turn.as_mut()?;
        if active.id != turn_id {
            return None;
        }
        if active
            .pending_exit_plan_mode
            .as_ref()
            .is_some_and(|pending| pending.request_id == request_id)
        {
            active.pending_exit_plan_mode.take()
        } else {
            None
        }
    }

    async fn fail_pending_ask_user_question(&self, turn_id: u64, message: &str) -> bool {
        let pending = {
            let mut state = self.state.lock().await;
            let Some(active) = state.active_turn.as_mut() else {
                return false;
            };
            if active.id != turn_id {
                return false;
            }
            active.pending_ask_user_question.take()
        };
        let Some(pending) = pending else {
            return false;
        };
        self.emit_tool_execution_completed(
            &pending.tool_call_id,
            &pending.tool_name,
            false,
            json!({
                "kind": "Error",
                "short_message": "AskUserQuestion failed",
                "detailed_message": message,
            }),
            Some(message.to_string()),
        );
        true
    }

    async fn fail_pending_exit_plan_mode(&self, turn_id: u64, message: &str) -> bool {
        let pending = {
            let mut state = self.state.lock().await;
            let Some(active) = state.active_turn.as_mut() else {
                return false;
            };
            if active.id != turn_id {
                return false;
            }
            active.pending_exit_plan_mode.take()
        };
        let Some(pending) = pending else {
            return false;
        };
        self.emit_tool_execution_completed(
            &pending.tool_call_id,
            &pending.tool_name,
            false,
            json!({
                "kind": "Error",
                "short_message": "ExitPlanMode failed",
                "detailed_message": message,
            }),
            Some(message.to_string()),
        );
        true
    }

    async fn ensure_process_ready(self: &Arc<Self>) -> Result<(), String> {
        if self.runtime.lock().await.is_some() {
            return Ok(());
        }

        let config = {
            let state = self.state.lock().await;
            ClaudeProcessSpawnConfig {
                workspace_root: state.workspace_root.clone(),
                ssh_host: state.ssh_host.clone(),
                session_id: if state.ephemeral {
                    None
                } else {
                    state.session_id.clone()
                },
                fork_from_session_id: if state.ephemeral {
                    None
                } else {
                    state.fork_from_session_id.clone()
                },
                resume_existing_session: !state.start_session_fresh,
                ephemeral: state.ephemeral,
                model: state.model.clone(),
                effort: state.effort,
                permission_mode: state.permission_mode.clone(),
                startup_mcp_config_json: state.startup_mcp_config_json.clone(),
                steering_content: state.steering_content.clone(),
                agent_identity: state.agent_identity.clone(),
                tool_policy: state.tool_policy.clone(),
            }
        };

        let runtime = self.spawn_process(config).await?;
        #[cfg(test)]
        let spawned_pid = {
            let mut child = runtime.child.lock().await;
            child.as_mut().and_then(|child| child.inner().id())
        };
        {
            let mut runtime_slot = self.runtime.lock().await;
            if runtime_slot.is_some() {
                drop(runtime_slot);
                runtime.kill().await;
                return Ok(());
            }
            *runtime_slot = Some(runtime);
        }
        #[cfg(test)]
        observe_claude_process_spawned(spawned_pid);

        match tokio::time::timeout(
            CLAUDE_INITIALIZE_TIMEOUT,
            self.send_control_request_with_timeout("initialize", CLAUDE_INITIALIZE_TIMEOUT),
        )
        .await
        {
            Ok(Ok(response)) => {
                self.configure_capacity_from_initialize(&response).await;
                self.schedule_capacity_refresh().await;
                Ok(())
            }
            Ok(Err(err)) => {
                self.shutdown_process().await;
                Err(err)
            }
            Err(_) => {
                self.shutdown_process().await;
                Err("Timed out initializing Claude CLI control protocol".to_string())
            }
        }
    }

    async fn configure_capacity_from_initialize(&self, response: &Value) {
        let access = claude_capacity_access_from_initialize(response);
        let emitter = {
            let mut state = self.state.lock().await;
            state.capacity_access = access;
            state.subagent_emitter.clone()
        };
        let Some(emitter) = emitter else {
            return;
        };
        let capacity = match access {
            ClaudeCapacityAccess::ApiKey => Some(protocol::BackendCapacityState::Unsupported {
                reason: protocol::CapacityUnsupportedReason::AccountTypeNotReported,
            }),
            ClaudeCapacityAccess::ExternalProvider => {
                Some(protocol::BackendCapacityState::Unsupported {
                    reason: protocol::CapacityUnsupportedReason::ExternalProvider,
                })
            }
            ClaudeCapacityAccess::Unknown | ClaudeCapacityAccess::Subscription => None,
        };
        if let Some(capacity) = capacity {
            emitter.on_backend_capacity(protocol::BackendKind::Claude, capacity);
        }
    }

    async fn schedule_capacity_refresh(self: &Arc<Self>) {
        let should_refresh = {
            let mut state = self.state.lock().await;
            if state.capacity_access != ClaudeCapacityAccess::Subscription
                || state.capacity_refresh_in_flight
            {
                false
            } else {
                state.capacity_refresh_in_flight = true;
                true
            }
        };
        if !should_refresh {
            return;
        }
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let result = this.send_control_request("get_usage").await;
            let capacity = match result {
                Ok(response) => map_claude_control_usage(&response),
                Err(_) => Err(CapacityUnavailableReason::SourceUnreachable),
            };
            let (emitter, should_emit) = {
                let mut state = this.state.lock().await;
                state.capacity_refresh_in_flight = false;
                let should_emit = capacity.is_ok() || !state.capacity_report_emitted;
                if capacity.is_ok() {
                    state.authoritative_capacity_emitted = true;
                    state.capacity_report_emitted = true;
                }
                (state.subagent_emitter.clone(), should_emit)
            };
            if should_emit && let Some(emitter) = emitter {
                let capacity = match capacity {
                    Ok(report) => protocol::BackendCapacityState::Known { report },
                    Err(reason) => protocol::BackendCapacityState::Unavailable { reason },
                };
                emitter.on_backend_capacity(protocol::BackendKind::Claude, capacity);
            }
        });
    }

    async fn handle_passive_capacity(self: &Arc<Self>, frame: &Value) {
        let (emitter, should_forward) = {
            let mut state = self.state.lock().await;
            let should_forward = !state.authoritative_capacity_emitted;
            if should_forward {
                state.capacity_report_emitted = true;
            }
            (state.subagent_emitter.clone(), should_forward)
        };
        if should_forward && let Some(emitter) = emitter {
            forward_passive_rate_limit_event(frame, emitter.as_ref());
        }
        self.schedule_capacity_refresh().await;
    }

    async fn spawn_process(
        self: &Arc<Self>,
        config: ClaudeProcessSpawnConfig,
    ) -> Result<ClaudeProcessRuntime, String> {
        let cli_args = build_claude_cli_args(&config);
        let mut child = if let Some(host) = config.ssh_host.as_deref() {
            crate::remote::spawn_remote_process(
                host,
                "claude",
                &cli_args,
                Some(&config.workspace_root),
            )
            .await
            .map_err(|err| format!("Failed to start Claude CLI over SSH: {err}"))?
        } else {
            let mut cmd = Command::new(claude_binary());
            for arg in &cli_args {
                cmd.arg(arg);
            }
            if let Some(path) = process_env::resolved_child_process_path() {
                cmd.env("PATH", path);
            }
            cmd.current_dir(&config.workspace_root)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
            cmd.group_spawn()
                .map_err(|err| format!("Failed to start Claude CLI: {err}"))?
        };

        let stdin = child
            .inner()
            .stdin
            .take()
            .ok_or_else(|| "Failed to capture Claude stdin".to_string())?;
        let stdout = child
            .inner()
            .stdout
            .take()
            .ok_or_else(|| "Failed to capture Claude stdout".to_string())?;
        let stderr = child
            .inner()
            .stderr
            .take()
            .ok_or_else(|| "Failed to capture Claude stderr".to_string())?;

        let stdin = Arc::new(Mutex::new(stdin));
        let child = Arc::new(Mutex::new(Some(child)));
        let control_waiters = Arc::new(Mutex::new(HashMap::new()));
        let stdout_task = tokio::spawn(read_claude_stdout_persistent(
            stdout,
            Arc::clone(self),
            Arc::clone(&control_waiters),
            Arc::clone(&stdin),
        ));
        let stderr_task = tokio::spawn(read_claude_stderr_persistent(stderr, Arc::clone(self)));

        Ok(ClaudeProcessRuntime {
            stdin,
            child,
            control_waiters,
            stdout_task,
            stderr_task,
        })
    }

    async fn write_process_json_line(&self, value: &Value) -> Result<(), String> {
        let stdin = {
            let runtime = self.runtime.lock().await;
            runtime
                .as_ref()
                .map(|runtime| Arc::clone(&runtime.stdin))
                .ok_or_else(|| "Claude CLI process is not running".to_string())?
        };
        write_json_line_to_stdin(&stdin, value).await
    }

    async fn send_control_request(&self, subtype: &str) -> Result<Value, String> {
        self.send_control_request_with_timeout(subtype, CLAUDE_CONTROL_RESPONSE_TIMEOUT)
            .await
    }

    async fn send_control_request_with_timeout(
        &self,
        subtype: &str,
        timeout_duration: Duration,
    ) -> Result<Value, String> {
        let request_id = uuid::Uuid::new_v4().to_string();
        let request = json!({
            "type": "control_request",
            "request_id": request_id,
            "request": {
                "subtype": subtype,
            },
        });
        self.send_control_request_value(request_id, request, timeout_duration)
            .await
    }

    async fn send_control_request_value(
        &self,
        request_id: String,
        value: Value,
        timeout_duration: Duration,
    ) -> Result<Value, String> {
        let (stdin, control_waiters) = {
            let runtime = self.runtime.lock().await;
            let runtime = runtime
                .as_ref()
                .ok_or_else(|| "Claude CLI process is not running".to_string())?;
            (
                Arc::clone(&runtime.stdin),
                Arc::clone(&runtime.control_waiters),
            )
        };

        let (tx, rx) = oneshot::channel();
        control_waiters.lock().await.insert(request_id.clone(), tx);
        if let Err(err) = write_json_line_to_stdin(&stdin, &value).await {
            control_waiters.lock().await.remove(&request_id);
            return Err(err);
        }

        match tokio::time::timeout(timeout_duration, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(format!(
                "Claude control request '{request_id}' was dropped before a response"
            )),
            Err(_) => {
                control_waiters.lock().await.remove(&request_id);
                Err(format!(
                    "Timed out waiting for Claude control response to '{request_id}'"
                ))
            }
        }
    }

    async fn active_turn_interrupted(&self, turn_id: u64) -> bool {
        let state = self.state.lock().await;
        state
            .active_turn
            .as_ref()
            .is_some_and(|active| active.id == turn_id && active.interrupt_requested)
    }

    async fn active_turn_pending_outcome_id(&self) -> Option<u64> {
        let state = self.state.lock().await;
        state.active_turn.as_ref().and_then(|active| {
            if active.outcome_tx.is_some() {
                Some(active.id)
            } else {
                None
            }
        })
    }

    async fn complete_active_turn_with_outcome(&self, turn_id: u64, outcome: TurnOutcome) -> bool {
        let tx = {
            let mut state = self.state.lock().await;
            let Some(active) = state.active_turn.as_mut() else {
                return false;
            };
            if active.id != turn_id {
                return false;
            }
            active.outcome_tx.take()
        };
        if let Some(tx) = tx {
            let _ = tx.send(outcome);
            true
        } else {
            false
        }
    }

    async fn take_restart_process_after_turn(&self) -> bool {
        let mut state = self.state.lock().await;
        let restart = state.restart_process_after_turn;
        state.restart_process_after_turn = false;
        restart
    }

    async fn shutdown_process(&self) {
        let runtime = self.runtime.lock().await.take();
        if let Some(runtime) = runtime {
            runtime.kill().await;
        }
    }

    async fn mark_process_exited(&self) {
        let runtime = self.runtime.lock().await.take();
        if let Some(runtime) = runtime {
            let mut child = runtime.child.lock().await;
            if let Some(child) = child.as_mut() {
                let _ = child.try_wait();
            }
        }
    }

    async fn cancel_active_turn(&self) {
        let (turn_id, quiesced_rx) = {
            let mut state = self.state.lock().await;
            let Some(active) = state.active_turn.as_mut() else {
                return;
            };
            let (quiesced_tx, quiesced_rx) = oneshot::channel();
            active.quiesced_waiters.push(quiesced_tx);
            active.interrupt_requested = true;
            (active.id, quiesced_rx)
        };

        if self.runtime.lock().await.is_some()
            && let Err(err) = self.send_control_request("interrupt").await
        {
            tracing::warn!("Failed to send Claude interrupt request: {err}");
        }

        match tokio::time::timeout(CLAUDE_INTERRUPT_QUIESCE_TIMEOUT, quiesced_rx).await {
            Ok(_) => {}
            Err(_) => {
                tracing::warn!(
                    "Claude did not quiesce after interrupt; killing persistent process"
                );
                self.shutdown_process().await;
                let fallback_rx = {
                    let mut state = self.state.lock().await;
                    state.active_turn.as_mut().and_then(|active| {
                        if active.id == turn_id {
                            let (tx, rx) = oneshot::channel();
                            active.quiesced_waiters.push(tx);
                            Some(rx)
                        } else {
                            None
                        }
                    })
                };
                self.complete_active_turn_with_outcome(
                    turn_id,
                    TurnOutcome::Cancelled {
                        summary: ClaudeStdoutSummary::default(),
                    },
                )
                .await;
                if let Some(rx) = fallback_rx {
                    let _ = rx.await;
                }
            }
        }
    }

    async fn clear_active_turn(&self, turn_id: u64) -> Vec<oneshot::Sender<()>> {
        let mut state = self.state.lock().await;
        if state
            .active_turn
            .as_ref()
            .is_some_and(|active| active.id == turn_id)
        {
            return state
                .active_turn
                .take()
                .map(|active| active.quiesced_waiters)
                .unwrap_or_default();
        }
        Vec::new()
    }

    /// Commit the Claude CLI session_id into backend state.
    ///
    /// Session ids are immutable for the lifetime of a `ClaudeBackend`. The
    /// first CLI session_id observed wins; any subsequent attempt to commit a
    /// different id is a protocol invariant violation (the Claude CLI rotated
    /// our session, which must never happen silently) and surfaces as a
    /// user-visible error.
    async fn set_session_id(&self, session_id: String) {
        let mut state = self.state.lock().await;
        match &state.session_id {
            Some(existing) if existing == &session_id => {
                state.fork_from_session_id = None;
                state.start_session_fresh = false;
            }
            Some(existing) => {
                let existing = existing.clone();
                drop(state);
                self.emit_error(&format!(
                    "Claude CLI rotated session id from {existing} to {session_id}; \
                     session ids must be immutable. This turn's output is orphaned."
                ));
            }
            None => {
                state.session_id = Some(session_id);
                state.fork_from_session_id = None;
                state.start_session_fresh = false;
            }
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
                state.effort.map(ClaudeEffort::as_str),
                state.permission_mode.clone(),
            )
        };

        self.emitter.settings(json!({
            "model": model,
            "effort": effort,
            // Alias for existing settings UI consumers.
            "reasoning_effort": effort,
            "permission_mode": permission_mode,
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
        self.emitter.sessions_list(sessions);
        Ok(())
    }

    async fn resume_session(&self, session_id: String) -> Result<(), String> {
        let normalized = normalize_nonempty(&session_id).ok_or("Invalid session id")?;
        self.shutdown_process().await;
        let (workspace_root, ssh_host) = {
            let mut state = self.state.lock().await;
            state.session_id = Some(normalized.clone());
            state.fork_from_session_id = None;
            state.start_session_fresh = false;
            state.cumulative_usage = None;
            state.cumulative_usage_complete = true;
            state.conversation_bytes_total = 0;
            (state.workspace_root.clone(), state.ssh_host.clone())
        };

        self.emitter.session_started(&normalized);
        self.emitter.conversation_cleared();
        self.emitter.typing_status_changed(false);

        let replay = match if let Some(host) = &ssh_host {
            load_claude_session_history_remote(host, &workspace_root, &normalized).await
        } else {
            load_claude_session_history(&workspace_root, &normalized).await
        } {
            Ok(replay) => replay,
            Err(err) if err.is_missing() => {
                self.recover_missing_session_history(&normalized, &workspace_root, &err)
                    .await;
                return Ok(());
            }
            Err(err) => return Err(err.to_string()),
        };
        for item in replay.items {
            match item {
                ClaudeHistoryReplayItem::Message(message) => {
                    self.emit_replay_message(message);
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
        state.cumulative_usage = replay.cumulative_usage;
        state.cumulative_usage_complete = replay.cumulative_usage_complete;
        state.conversation_bytes_total = replay.conversation_bytes_total;
        state.start_session_fresh = false;
        Ok(())
    }

    async fn recover_missing_session_history(
        &self,
        session_id: &str,
        workspace_root: &str,
        error: &ClaudeSessionHistoryError,
    ) {
        tracing::warn!(
            session_id = %session_id,
            workspace_root = %workspace_root,
            error = %error,
            "Claude session history is missing; starting a fresh Claude CLI session with the same id"
        );
        {
            let mut state = self.state.lock().await;
            state.session_id = Some(session_id.to_string());
            state.fork_from_session_id = None;
            state.start_session_fresh = true;
            state.cumulative_usage = None;
            state.cumulative_usage_complete = true;
            state.conversation_bytes_total = 0;
        }
        self.emitter.warning_message(&format!(
            "Claude session history for '{session_id}' is no longer available. Starting a fresh Claude session."
        ));
    }

    async fn delete_session(&self, session_id: String) -> Result<(), String> {
        let normalized = normalize_nonempty(&session_id).ok_or("Invalid session id")?;
        self.shutdown_process().await;
        let (workspace_root, ssh_host) = {
            let mut state = self.state.lock().await;
            if state.session_id.as_deref() == Some(normalized.as_str()) {
                state.session_id = None;
                state.fork_from_session_id = None;
                state.start_session_fresh = false;
                state.cumulative_usage = None;
                state.cumulative_usage_complete = true;
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
            && let Some(tasks) = claude_task_update_from_todo_write(&tool_call.arguments)
        {
            self.emitter.task_update(&tasks);
        }
        if let Some(progress) =
            await_progress_data_for_tool(&tool_call.id, &tool_call.name, &tool_call.arguments)
        {
            self.emitter.tool_progress(&progress);
        }
        let (tool_type, normalization_failure) =
            match tyde_tool_request_type(&tool_call.name, &tool_call.arguments) {
                Ok(Some(tool_type)) => (
                    serde_json::to_value(tool_type).expect("serialize tool request"),
                    None,
                ),
                Ok(None) => (
                    claude_tool_request_type(&tool_call.name, &tool_call.arguments),
                    None,
                ),
                Err(error) => {
                    tracing::error!(
                        tool = %tool_call.name,
                        tool_call_id = %tool_call.id,
                        detail = %error.detail,
                        "Canonical Tyde tool request normalization failed"
                    );
                    self.emitter.backend_error(&format!(
                        "Failed to normalize canonical tool request '{}' ({}): {}",
                        tool_call.name, tool_call.id, error
                    ));
                    (
                        claude_tool_request_type(&tool_call.name, &tool_call.arguments),
                        Some(error.normalization_failure),
                    )
                }
            };
        if let Some(normalization_failure) = normalization_failure {
            self.emitter.tool_request_with_normalization_failure(
                &tool_call.id,
                &tool_call.name,
                tool_type,
                normalization_failure,
            );
        } else {
            self.emitter
                .tool_request(&tool_call.id, &tool_call.name, tool_type);
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
        if success
            && let Some(progress) =
                spawn_progress_data_for_tool_result(tool_call_id, tool_name, &tool_result)
        {
            self.emitter.tool_progress(&progress);
        }
        let (tool_result, normalization_failure) = if success {
            match tyde_tool_result(tool_name, &tool_result) {
                Ok(Some(typed)) => (
                    serde_json::to_value(typed).expect("serialize tool result"),
                    None,
                ),
                Ok(None) => (tool_result, None),
                Err(normalize_error) => {
                    tracing::error!(
                        tool = %tool_name,
                        tool_call_id = %tool_call_id,
                        detail = %normalize_error.detail,
                        "Canonical Tyde tool result normalization failed"
                    );
                    self.emitter.backend_error(&format!(
                        "Failed to normalize canonical tool result '{}' ({}): {}",
                        tool_name, tool_call_id, normalize_error
                    ));
                    (tool_result, Some(normalize_error.normalization_failure))
                }
            }
        } else {
            (tool_result, None)
        };
        let completed = ToolCompletedPayload {
            tool_call_id,
            tool_name,
            tool_result,
            success,
            error: error.as_deref(),
        };
        if let Some(normalization_failure) = normalization_failure {
            self.emitter
                .tool_completed_with_normalization_failure(completed, normalization_failure);
        } else {
            self.emitter.tool_completed(completed);
        }
    }

    async fn shutdown(&self) {
        self.cancel_active_turn().await;
        self.shutdown_process().await;
    }

    fn emit_typing_status(&self, typing: bool) {
        self.emitter.typing_status_changed(typing);
    }

    fn emit_stream_start(&self, message_id: &str, model: Option<String>) {
        self.emitter
            .stream_start(message_id, AgentName(CLAUDE_AGENT_NAME), model.as_deref());
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
        self.emitter.user_message(content, image_payload);
    }

    fn emit_stream_delta(&self, message_id: &str, text: &str) {
        self.emitter.stream_delta(message_id, text);
    }

    fn emit_stream_reasoning_delta(&self, message_id: &str, text: &str) {
        self.emitter.stream_reasoning_delta(message_id, text);
    }

    fn emit_system_message(&self, content: &str) {
        self.emitter.system_message(content);
    }

    fn emit_stream_end(
        &self,
        content: String,
        model: Option<String>,
        usage: ClaudeMessageUsage,
        reasoning: Option<String>,
        tool_calls: Vec<Value>,
        context_breakdown: Option<Value>,
    ) {
        let token_usage_unavailable_reason = (usage.turn.is_some() && usage.cumulative.is_none())
            .then_some(protocol::TokenUsageUnavailableReason::ProviderScopeAmbiguous);
        self.emitter.stream_end(StreamEndPayload {
            content,
            agent: Some(AgentName(CLAUDE_AGENT_NAME)),
            model,
            request_usage: usage.request,
            turn_usage: usage.turn,
            cumulative_usage: usage.cumulative,
            token_usage_unavailable_reason,
            reasoning,
            tool_calls,
            context_breakdown,
        });
    }

    fn emit_placeholder_stream_end(
        &self,
        model: Option<String>,
        turn_usage: Option<ClaudeTurnUsage>,
        context_breakdown: Option<Value>,
    ) {
        self.emit_stream_end(
            String::new(),
            model,
            ClaudeMessageUsage {
                request: None,
                turn: turn_usage.as_ref().map(|usage| usage.turn.clone()),
                cumulative: turn_usage.and_then(|usage| usage.cumulative),
            },
            None,
            Vec::new(),
            context_breakdown,
        );
    }

    fn emit_operation_cancelled(&self, message: &str) {
        self.emitter.operation_cancelled(message);
    }

    /// Re-emit a persisted message from the session replay. Dispatches
    /// to the right typed method on the emitter based on the sender
    /// shape. User messages carry an image list; assistant messages
    /// carry reasoning / tool_calls / usage.
    fn emit_replay_message(&self, message: Value) {
        let sender = message.get("sender");
        if sender.and_then(Value::as_str) == Some("User") {
            let content = message
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let images = message
                .get("images")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            self.emitter.user_message(content, images);
            return;
        }

        // Anything non-User during replay is an assistant message.
        let content = message
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let reasoning = message.get("reasoning").cloned().filter(|v| !v.is_null());
        let tool_calls = message
            .get("tool_calls")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let model_info = message.get("model_info").cloned().filter(|v| !v.is_null());
        let token_usage = message.get("token_usage").cloned().filter(|v| !v.is_null());
        let context_breakdown = message
            .get("context_breakdown")
            .cloned()
            .filter(|v| !v.is_null());
        let images = message
            .get("images")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        self.emitter.assistant_message(AssistantMessagePayload {
            agent: AgentName(CLAUDE_AGENT_NAME),
            message_id: None,
            content,
            reasoning,
            tool_calls,
            model_info,
            request_usage: token_usage,
            turn_usage: None,
            cumulative_usage: None,
            context_breakdown,
            images,
        });
    }

    fn emit_error(&self, message: &str) {
        self.emitter.backend_error(message);
    }

    async fn normalize_usage_for_turn(&self, usage: Option<Value>) -> Option<ClaudeTurnUsage> {
        let turn = usage?;

        let mut state = self.state.lock().await;
        let cumulative = add_token_usage(state.cumulative_usage.as_ref(), &turn);
        state.cumulative_usage = Some(cumulative.clone());
        let cumulative = state.cumulative_usage_complete.then_some(cumulative);
        Some(ClaudeTurnUsage { turn, cumulative })
    }

    async fn emit_terminal_phase_or_placeholder(
        &self,
        summary: &mut ClaudeStdoutSummary,
        conversation_history_bytes: u64,
        known_context_window: Option<u64>,
        model_hint: Option<String>,
        turn_usage: Option<ClaudeTurnUsage>,
    ) -> bool {
        // The Context Usage breakdown must reflect the context-window fill — the
        // last API call's prompt footprint — which lives on `summary.usage`
        // (per-API-call usage from assistant stream events, bounded by the
        // window). It is NOT `turn_usage`: that is the per-turn delta of Claude's
        // session-cumulative counter, i.e. the sum of input tokens across every
        // API call in the turn, which overflows the window on multi-step turns
        // (a turn re-sends its growing context on each request). Capture the
        // per-call value before `take_phase_emission` consumes `summary.usage`.
        let context_usage = summary.usage.clone();
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
                context_usage.as_ref(),
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
                ClaudeMessageUsage {
                    request: phase.usage,
                    turn: turn_usage.as_ref().map(|usage| usage.turn.clone()),
                    cumulative: turn_usage
                        .as_ref()
                        .and_then(|usage| usage.cumulative.clone()),
                },
                phase.reasoning,
                tool_calls,
                Some(context_breakdown),
            );
            for tool_call in &phase.tool_calls {
                emit_tool_request_with_tracking(summary, self, tool_call);
            }
            auto_close_unresolved_tool_requests(
                summary,
                self,
                "Claude ended the turn before returning a result for this streamed tool request.",
            );
            return true;
        }

        if let Some(control_event) = summary.control_event {
            if summary.emitted_phase_count == 0 {
                let selected_model = summary.model.clone().or(model_hint);
                let context_breakdown = estimate_context_breakdown(
                    context_usage.as_ref(),
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
                self.emit_placeholder_stream_end(
                    selected_model,
                    turn_usage.clone(),
                    Some(context_breakdown),
                );
            }
            auto_close_unresolved_tool_requests(
                summary,
                self,
                "Claude ended the turn before returning a result for this streamed tool request.",
            );
            return true;
        }

        // Close any still-open stream BEFORE emitting tool cleanups, so the
        // ordering matches the protocol spec (StreamEnd → ToolExecutionCompleted
        // → OperationCancelled). `emitted_phase_count == 0` catches the
        // no-content-yet case; `emitter.is_stream_open()` catches a mid-turn
        // segment that emitted StreamStart without any content before cancel.
        if summary.emitted_phase_count == 0 || self.emitter.is_stream_open() {
            let selected_model = summary.model.clone().or(model_hint);
            let context_breakdown = estimate_context_breakdown(
                context_usage.as_ref(),
                conversation_history_bytes,
                summary.tool_io_bytes,
                summary.reasoning_bytes,
                known_context_window,
                selected_model.as_deref(),
            );
            self.emit_placeholder_stream_end(selected_model, turn_usage, Some(context_breakdown));
        }

        if !summary.unresolved_tool_requests.is_empty() {
            auto_close_unresolved_tool_requests(
                summary,
                self,
                "Claude ended the turn before returning a result for this streamed tool request.",
            );
            return true;
        }

        false
    }
}

fn claude_binary() -> String {
    std::env::var(TYDE_CLAUDE_BIN_ENV)
        .ok()
        .and_then(|value| normalize_nonempty(&value))
        .unwrap_or_else(|| "claude".to_string())
}

async fn write_json_line_to_stdin(
    stdin: &Arc<Mutex<ChildStdin>>,
    value: &Value,
) -> Result<(), String> {
    let payload = serde_json::to_string(value)
        .map_err(|err| format!("Failed to encode Claude input payload: {err}"))?;
    let mut stdin = stdin.lock().await;
    stdin
        .write_all(payload.as_bytes())
        .await
        .map_err(|err| format!("Failed to write Claude input: {err}"))?;
    stdin
        .write_all(b"\n")
        .await
        .map_err(|err| format!("Failed to finalize Claude input: {err}"))?;
    stdin
        .flush()
        .await
        .map_err(|err| format!("Failed to flush Claude input: {err}"))?;
    Ok(())
}

fn build_claude_cli_args(config: &ClaudeProcessSpawnConfig) -> Vec<String> {
    let effective_permission_mode = config
        .permission_mode
        .as_deref()
        .unwrap_or(CLAUDE_DEFAULT_PERMISSION_MODE);
    let mut cli_args: Vec<String> = vec![
        "--print".to_string(),
        "--verbose".to_string(),
        "--output-format".to_string(),
        "stream-json".to_string(),
        "--input-format".to_string(),
        "stream-json".to_string(),
        "--include-partial-messages".to_string(),
        "--permission-prompt-tool".to_string(),
        "stdio".to_string(),
        "--permission-mode".to_string(),
        effective_permission_mode.to_string(),
    ];

    if config.ephemeral {
        cli_args.push("--no-session-persistence".to_string());
    }

    if effective_permission_mode.eq_ignore_ascii_case("bypassPermissions") {
        cli_args.push("--dangerously-skip-permissions".to_string());
    }

    if let Some(model_name) = config.model.as_deref().and_then(normalize_nonempty) {
        cli_args.push("--model".to_string());
        cli_args.push(model_name);
    }

    if let Some(effort_level) = config.effort {
        cli_args.push("--effort".to_string());
        cli_args.push(effort_level.as_str().to_string());
    }

    if let Some(mcp_config_json) = config
        .startup_mcp_config_json
        .as_deref()
        .and_then(normalize_nonempty)
    {
        cli_args.push("--mcp-config".to_string());
        cli_args.push(mcp_config_json);
    }

    match &config.tool_policy {
        ToolPolicy::Unrestricted => {}
        ToolPolicy::AllowList { tools } => {
            cli_args.push("--allowedTools".to_string());
            cli_args.extend(tools.iter().cloned());
        }
        ToolPolicy::DenyList { tools } => {
            cli_args.push("--disallowedTools".to_string());
            cli_args.extend(tools.iter().cloned());
        }
    }

    if let Some(identity) = &config.agent_identity {
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

    if let Some(steering) = config
        .steering_content
        .as_deref()
        .and_then(normalize_nonempty)
    {
        cli_args.push("--append-system-prompt".to_string());
        cli_args.push(steering);
    }

    if !config.ephemeral
        && let Some(parent_session) = config
            .fork_from_session_id
            .as_deref()
            .and_then(normalize_nonempty)
    {
        cli_args.push("--resume".to_string());
        cli_args.push(parent_session);
        cli_args.push("--fork-session".to_string());
    } else if !config.ephemeral
        && let Some(existing_session) = config.session_id.as_deref().and_then(normalize_nonempty)
    {
        if config.resume_existing_session {
            cli_args.push("--resume".to_string());
        } else {
            cli_args.push("--session-id".to_string());
        }
        cli_args.push(existing_session);
    } else {
        cli_args.push("--session-id".to_string());
        cli_args.push(uuid::Uuid::new_v4().to_string());
    }

    cli_args
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

fn route_subagent_event(
    streams: &mut HashMap<String, SubAgentStream>,
    parent_id: &str,
    value: &Value,
) -> Result<(), String> {
    let Some(stream) = streams.get_mut(parent_id) else {
        let event_type = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        return Err(format!(
            "Claude child event could not be routed: parent_tool_use_id={parent_id}, event_type={event_type}"
        ));
    };
    consume_subagent_event(stream, value);
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SubAgentCorrelation {
    Live,
    Orphaned,
    Unowned,
}

fn classify_subagent_correlation(
    streams: &HashMap<String, SubAgentStream>,
    known_subagent_ids: &HashSet<String>,
    parent_id: &str,
) -> SubAgentCorrelation {
    if streams.contains_key(parent_id) {
        SubAgentCorrelation::Live
    } else if known_subagent_ids.contains(parent_id) {
        SubAgentCorrelation::Orphaned
    } else {
        SubAgentCorrelation::Unowned
    }
}

fn handle_correlated_subagent_event(
    streams: &mut HashMap<String, SubAgentStream>,
    known_subagent_ids: &HashSet<String>,
    parent_emitter: &TurnEmitter,
    parent_id: &str,
    value: &Value,
) {
    match classify_subagent_correlation(streams, known_subagent_ids, parent_id) {
        SubAgentCorrelation::Live => {
            route_subagent_event(streams, parent_id, value)
                .expect("live Claude child correlation must route");
        }
        SubAgentCorrelation::Orphaned => {
            let event_type = value
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let message = format!(
                "Claude child event arrived after its stream closed: parent_tool_use_id={parent_id}, event_type={event_type}"
            );
            tracing::error!("{message}");
            parent_emitter.subprocess_stderr(&message);
        }
        SubAgentCorrelation::Unowned => {
            let event_type = value
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            tracing::debug!(
                parent_tool_use_id = parent_id,
                event_type,
                "ignoring correlated Claude frame not owned by a sub-agent"
            );
        }
    }
}

/// Whether a Task/Agent tool_use block requested background execution.
fn extract_run_in_background(block: &Value) -> Option<bool> {
    block
        .get("input")
        .and_then(|input| input.get("run_in_background"))
        .and_then(Value::as_bool)
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

#[derive(Default)]
struct PersistentStdoutTurnState {
    active_turn_id: Option<u64>,
    base_message_id: String,
    current_message_id: String,
    summary: ClaudeStdoutSummary,
    segment: SegmentState,
}

async fn read_claude_stdout_persistent(
    stdout: ChildStdout,
    inner: Arc<ClaudeInner>,
    control_waiters: ClaudeControlWaiters,
    stdin: Arc<Mutex<ChildStdin>>,
) {
    let mut turn_state = PersistentStdoutTurnState::default();
    let mut lines = BufReader::new(stdout).lines();
    let mut subagent_streams: HashMap<String, SubAgentStream> = HashMap::new();
    let mut known_subagent_ids = HashSet::new();
    let mut pending_subagent_prompts: HashMap<u64, PendingSubAgentPrompt> = HashMap::new();
    // Keyed by task_id; lives at loop scope (not per-turn) because a
    // workflow's task frames keep arriving after its turn completes.
    let mut workflow_runs: HashMap<String, WorkflowRunEntry> = HashMap::new();

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

        if route_control_response(&value, &control_waiters).await {
            continue;
        }
        if handle_exit_plan_mode_control_request(&value, &inner, &mut turn_state, &stdin).await {
            continue;
        }
        if handle_ask_user_question_control_request(&value, &inner, &mut turn_state, &stdin).await {
            continue;
        }
        if respond_to_control_request(&value, &stdin).await {
            continue;
        }

        if handle_workflow_task_frame(&value, &mut workflow_runs, &inner.emitter) {
            continue;
        }

        let subagent_emitter = {
            let state = inner.state.lock().await;
            state.subagent_emitter.clone()
        };

        if value.get("type").and_then(Value::as_str) == Some("rate_limit_event") {
            inner.handle_passive_capacity(&value).await;
            continue;
        }

        if let Some(ref emitter) = subagent_emitter {
            detect_subagent_task_system_spawns(
                &value,
                emitter.as_ref(),
                &inner.emitter,
                &mut subagent_streams,
            )
            .await;
            known_subagent_ids.extend(subagent_streams.keys().cloned());
            // A background sub-agent completes via `task_notification`, which
            // arrives on the parent stream after the parent's turn `result`.
            // Handle it pre-gate so it lands even with no active turn.
            finalize_background_subagent_completion(&value, &mut subagent_streams);
        }

        if let Some(parent_id) = extract_parent_tool_use_id(&value) {
            handle_correlated_subagent_event(
                &mut subagent_streams,
                &known_subagent_ids,
                &inner.emitter,
                parent_id,
                &value,
            );
            continue;
        }

        if let Some(ref emitter) = subagent_emitter {
            detect_subagent_spawns(
                &value,
                emitter.as_ref(),
                &inner.emitter,
                &mut subagent_streams,
                &mut pending_subagent_prompts,
            )
            .await;
            known_subagent_ids.extend(subagent_streams.keys().cloned());
        }

        let _turn_event_guard = inner.turn_event_gate.lock().await;
        let (turn_id, model_hint) =
            match prepare_persistent_stdout_turn(&inner, &mut turn_state).await {
                Some(turn) => turn,
                None => {
                    // No user-initiated turn is active, yet the CLI is emitting
                    // fresh turn output. This happens when the model resumes on
                    // its own after a background sub-agent finishes: the parent
                    // turn's `result` already completed, then a new `init` +
                    // assistant + `result` sequence arrives. Adopt it as a
                    // first-class turn so the follow-up isn't silently dropped.
                    if !is_cli_turn_start_event(&value) {
                        continue;
                    }
                    if inner.begin_cli_initiated_turn().await.is_none() {
                        continue;
                    }
                    match prepare_persistent_stdout_turn(&inner, &mut turn_state).await {
                        Some(turn) => turn,
                        None => continue,
                    }
                }
            };

        consume_claude_stream_value(
            &value,
            &mut turn_state.summary,
            &mut turn_state.segment,
            &inner,
            &turn_state.base_message_id,
            &mut turn_state.current_message_id,
        );

        if subagent_emitter.is_some() {
            detect_subagent_completions(&value, &mut subagent_streams).await;
        }

        if value.get("type").and_then(Value::as_str) == Some("result") {
            flush_pending_tool_uses(&mut turn_state.summary, &mut turn_state.segment);
            let summary = std::mem::take(&mut turn_state.summary);
            turn_state.segment = SegmentState::default();
            turn_state.active_turn_id = None;
            turn_state.base_message_id.clear();
            turn_state.current_message_id.clear();

            let interrupted = inner.active_turn_interrupted(turn_id).await;
            let outcome = claude_result_turn_outcome(&value, summary, model_hint, interrupted);
            inner
                .complete_active_turn_with_outcome(turn_id, outcome)
                .await;
        }
    }

    for (_tool_use_id, stream) in subagent_streams.drain() {
        finalize_subagent_stream(stream);
    }

    fail_pending_control_waiters(&control_waiters, "Claude CLI process exited").await;
    let _turn_event_guard = inner.turn_event_gate.lock().await;
    let active_turn_id = if let Some(turn_id) = turn_state.active_turn_id {
        Some(turn_id)
    } else {
        inner.active_turn_pending_outcome_id().await
    };
    if let Some(turn_id) = active_turn_id {
        if turn_state.active_turn_id.is_some() {
            flush_pending_tool_uses(&mut turn_state.summary, &mut turn_state.segment);
        }
        let summary = std::mem::take(&mut turn_state.summary);
        let interrupted = inner.active_turn_interrupted(turn_id).await;
        let outcome = if interrupted {
            TurnOutcome::Cancelled { summary }
        } else {
            TurnOutcome::Failed {
                summary,
                error: "Claude process exited before returning a result".to_string(),
            }
        };
        inner
            .complete_active_turn_with_outcome(turn_id, outcome)
            .await;
    }
    inner.mark_process_exited().await;
}

/// Whether a parent-stream frame (already excluded from sub-agent routing)
/// marks the start of fresh turn content. Used to decide when to adopt
/// CLI-initiated output as a new turn. Deliberately excludes lone `result`
/// and `user` frames so a stray terminal frame never spawns an empty turn.
fn is_cli_turn_start_event(value: &Value) -> bool {
    let event_type = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match event_type {
        "assistant" | "stream_event" | "event" => true,
        "system" => value.get("subtype").and_then(Value::as_str) == Some("init"),
        other => is_stream_event_type(other),
    }
}

async fn prepare_persistent_stdout_turn(
    inner: &Arc<ClaudeInner>,
    turn_state: &mut PersistentStdoutTurnState,
) -> Option<(u64, Option<String>)> {
    let (turn_id, model_hint) = {
        let state = inner.state.lock().await;
        let active = state.active_turn.as_ref()?;
        (active.id, state.model.clone())
    };

    if turn_state.active_turn_id != Some(turn_id) {
        let base_message_id = format!("claude-msg-{turn_id}");
        turn_state.active_turn_id = Some(turn_id);
        turn_state.base_message_id = base_message_id.clone();
        turn_state.current_message_id = base_message_id;
        turn_state.summary = ClaudeStdoutSummary::default();
        turn_state.segment = SegmentState::default();
    }

    Some((turn_id, model_hint))
}

fn claude_result_turn_outcome(
    value: &Value,
    summary: ClaudeStdoutSummary,
    model_hint: Option<String>,
    interrupted: bool,
) -> TurnOutcome {
    if interrupted {
        return TurnOutcome::Cancelled { summary };
    }

    let is_error = value
        .get("is_error")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || value.get("subtype").and_then(Value::as_str) == Some("error");
    if is_error {
        let error = summary
            .error_message()
            .or_else(|| extract_result_error(value))
            .unwrap_or_else(|| "Claude returned an error result".to_string());
        return TurnOutcome::Failed { summary, error };
    }

    TurnOutcome::Completed {
        summary,
        model_hint,
    }
}

async fn route_control_response(value: &Value, control_waiters: &ClaudeControlWaiters) -> bool {
    if value.get("type").and_then(Value::as_str) != Some("control_response") {
        return false;
    }
    let Some(request_id) = control_response_request_id(value) else {
        tracing::warn!("Ignoring Claude control_response without request_id: {value}");
        return true;
    };
    let result = if control_response_is_success(value) {
        Ok(value
            .get("response")
            .and_then(|response| response.get("response"))
            .cloned()
            .unwrap_or(Value::Null))
    } else {
        Err(control_response_error(value))
    };
    if let Some(waiter) = control_waiters.lock().await.remove(&request_id) {
        let _ = waiter.send(result);
    } else {
        tracing::debug!("Dropping unmatched Claude control_response request_id={request_id}");
    }
    true
}

fn control_response_request_id(value: &Value) -> Option<String> {
    value
        .get("request_id")
        .and_then(Value::as_str)
        .or_else(|| {
            value
                .get("response")
                .and_then(|response| response.get("request_id"))
                .and_then(Value::as_str)
        })
        .and_then(normalize_nonempty)
}

fn control_response_is_success(value: &Value) -> bool {
    value
        .get("response")
        .and_then(|response| response.get("subtype"))
        .and_then(Value::as_str)
        == Some("success")
}

fn control_response_error(value: &Value) -> String {
    value
        .get("response")
        .and_then(|response| response.get("error"))
        .and_then(|error| {
            error
                .get("message")
                .and_then(Value::as_str)
                .or_else(|| error.as_str())
        })
        .and_then(normalize_nonempty)
        .unwrap_or_else(|| format!("Claude control request failed: {value}"))
}

async fn handle_ask_user_question_control_request(
    value: &Value,
    inner: &Arc<ClaudeInner>,
    turn_state: &mut PersistentStdoutTurnState,
    stdin: &Arc<Mutex<ChildStdin>>,
) -> bool {
    let Some(request) = ask_user_question_control_request(value) else {
        return false;
    };
    let request_id = request.request_id.clone();
    let _turn_event_guard = inner.turn_event_gate.lock().await;
    let result = bridge_ask_user_question_control_request(inner, turn_state, request).await;
    if let Err(err) = result {
        tracing::warn!("Failed to bridge Claude AskUserQuestion control_request: {err}");
        let payload = tool_permission_control_response_payload(
            &request_id,
            json!({
                "behavior": "deny",
                "message": err,
            }),
        );
        if let Err(write_err) = write_json_line_to_stdin(stdin, &payload).await {
            tracing::warn!("Failed to write Claude AskUserQuestion deny response: {write_err}");
        }
    }
    true
}

async fn handle_exit_plan_mode_control_request(
    value: &Value,
    inner: &Arc<ClaudeInner>,
    turn_state: &mut PersistentStdoutTurnState,
    stdin: &Arc<Mutex<ChildStdin>>,
) -> bool {
    let Some(request) = exit_plan_mode_control_request(value) else {
        return false;
    };
    let request_id = request.request_id.clone();
    let _turn_event_guard = inner.turn_event_gate.lock().await;
    let result = bridge_exit_plan_mode_control_request(inner, turn_state, request).await;
    if let Err(err) = result {
        tracing::warn!("Failed to bridge Claude ExitPlanMode control_request: {err}");
        let payload = tool_permission_control_response_payload(
            &request_id,
            json!({
                "behavior": "deny",
                "message": err,
            }),
        );
        if let Err(write_err) = write_json_line_to_stdin(stdin, &payload).await {
            tracing::warn!("Failed to write Claude ExitPlanMode deny response: {write_err}");
        }
    }
    true
}

async fn respond_to_control_request(value: &Value, stdin: &Arc<Mutex<ChildStdin>>) -> bool {
    let Some(payload) = control_response_payload_for_request(value) else {
        return false;
    };
    if payload.is_null() {
        return true;
    }
    if let Err(err) = write_json_line_to_stdin(stdin, &payload).await {
        tracing::warn!("Failed to write Claude control_response: {err}");
    }
    true
}

async fn bridge_ask_user_question_control_request(
    inner: &Arc<ClaudeInner>,
    turn_state: &mut PersistentStdoutTurnState,
    request: AskUserQuestionControlRequest,
) -> Result<(), String> {
    prepare_persistent_stdout_turn(inner, turn_state)
        .await
        .ok_or_else(|| "Claude asked a question with no active turn".to_string())?;

    let tool_call = ensure_ask_user_question_tool_request_emitted(
        &mut turn_state.summary,
        &mut turn_state.segment,
        inner,
        request.clone(),
    );
    inner
        .begin_ask_user_question_control_request(AskUserQuestionControlRequest {
            tool_call_id: tool_call.id,
            tool_name: tool_call.name,
            input: tool_call.arguments,
            ..request
        })
        .await
}

async fn bridge_exit_plan_mode_control_request(
    inner: &Arc<ClaudeInner>,
    turn_state: &mut PersistentStdoutTurnState,
    request: ExitPlanModeControlRequest,
) -> Result<(), String> {
    prepare_persistent_stdout_turn(inner, turn_state)
        .await
        .ok_or_else(|| "Claude requested plan approval with no active turn".to_string())?;

    let tool_call = ensure_exit_plan_mode_tool_request_emitted(
        &mut turn_state.summary,
        &mut turn_state.segment,
        inner,
        request.clone(),
    );
    inner
        .begin_exit_plan_mode_control_request(ExitPlanModeControlRequest {
            tool_call_id: tool_call.id,
            tool_name: tool_call.name,
            input: tool_call.arguments,
            ..request
        })
        .await
}

fn ensure_ask_user_question_tool_request_emitted(
    summary: &mut ClaudeStdoutSummary,
    segment: &mut SegmentState,
    inner: &ClaudeInner,
    request: AskUserQuestionControlRequest,
) -> ClaudeToolCall {
    flush_pending_tool_uses(summary, segment);

    let mut tool_call = summary
        .tool_call_by_id
        .get(&request.tool_call_id)
        .cloned()
        .unwrap_or_else(|| ClaudeToolCall {
            id: request.tool_call_id.clone(),
            name: request.tool_name.clone(),
            arguments: request.input.clone(),
        });

    if !has_meaningful_tool_arguments(&tool_call.arguments) {
        tool_call.arguments = request.input.clone();
        if let Some(existing) = summary
            .tool_calls
            .iter_mut()
            .find(|existing| existing.id == tool_call.id)
        {
            existing.arguments = tool_call.arguments.clone();
        }
        summary
            .tool_call_by_id
            .insert(tool_call.id.clone(), tool_call.clone());
    }

    let already_emitted = summary.unresolved_tool_requests.contains_key(&tool_call.id);
    let in_current_phase = summary
        .tool_calls
        .iter()
        .any(|tool| tool.id == tool_call.id);
    if !already_emitted && !in_current_phase {
        register_tool_call_for_phase(summary, segment, tool_call.clone());
    }

    let mut emitted = already_emitted;
    if !emitted {
        if phase_has_pending_output(summary, segment) {
            close_current_phase(summary, segment, inner);
        }
        emitted = summary
            .unresolved_tool_requests
            .remove(&tool_call.id)
            .is_some();
    } else {
        summary.unresolved_tool_requests.remove(&tool_call.id);
    }

    if !emitted {
        inner.emit_stream_end(
            String::new(),
            None,
            ClaudeMessageUsage::default(),
            None,
            vec![json!({
                "id": tool_call.id,
                "name": tool_call.name,
                "arguments": tool_call.arguments,
            })],
            None,
        );
        inner.emit_tool_request(&tool_call);
    }

    tool_call
}

fn ensure_exit_plan_mode_tool_request_emitted(
    summary: &mut ClaudeStdoutSummary,
    segment: &mut SegmentState,
    inner: &ClaudeInner,
    request: ExitPlanModeControlRequest,
) -> ClaudeToolCall {
    enrich_exit_plan_mode_tool_calls(summary);
    flush_pending_tool_uses(summary, segment);
    enrich_exit_plan_mode_tool_calls(summary);

    let request_input = enrich_exit_plan_mode_arguments(
        request.input.clone(),
        exit_plan_mode_plan_info_from_tool_calls(summary.tool_call_by_id.values()),
    );
    let mut tool_call = summary
        .tool_call_by_id
        .get(&request.tool_call_id)
        .cloned()
        .unwrap_or_else(|| ClaudeToolCall {
            id: request.tool_call_id.clone(),
            name: request.tool_name.clone(),
            arguments: request_input.clone(),
        });

    let existing_info = exit_plan_mode_plan_info_from_arguments(&tool_call.arguments);
    if !has_meaningful_tool_arguments(&tool_call.arguments)
        || (existing_info.plan.is_none() && existing_info.plan_path.is_none())
    {
        tool_call.arguments = request_input.clone();
        if let Some(existing) = summary
            .tool_calls
            .iter_mut()
            .find(|existing| existing.id == tool_call.id)
        {
            existing.arguments = tool_call.arguments.clone();
        }
        summary
            .tool_call_by_id
            .insert(tool_call.id.clone(), tool_call.clone());
    }

    let already_emitted = summary.unresolved_tool_requests.contains_key(&tool_call.id);
    let in_current_phase = summary
        .tool_calls
        .iter()
        .any(|tool| tool.id == tool_call.id);
    if !already_emitted && !in_current_phase {
        register_tool_call_for_phase(summary, segment, tool_call.clone());
    }

    let mut emitted = already_emitted;
    if !emitted {
        if phase_has_pending_output(summary, segment) {
            close_current_phase(summary, segment, inner);
        }
        emitted = summary
            .unresolved_tool_requests
            .remove(&tool_call.id)
            .is_some();
    } else {
        summary.unresolved_tool_requests.remove(&tool_call.id);
    }

    if !emitted {
        inner.emit_stream_end(
            String::new(),
            None,
            ClaudeMessageUsage::default(),
            None,
            vec![json!({
                "id": tool_call.id,
                "name": tool_call.name,
                "arguments": tool_call.arguments,
            })],
            None,
        );
        inner.emit_tool_request(&tool_call);
    }

    tool_call
}

fn control_response_payload_for_request(value: &Value) -> Option<Value> {
    if value.get("type").and_then(Value::as_str) != Some("control_request") {
        return None;
    }
    let request = value.get("request").unwrap_or(&Value::Null);
    let request_id = value
        .get("request_id")
        .and_then(Value::as_str)
        .or_else(|| request.get("request_id").and_then(Value::as_str))
        .and_then(normalize_nonempty);
    let Some(request_id) = request_id else {
        tracing::warn!("Ignoring Claude control_request without request_id: {value}");
        return Some(Value::Null);
    };
    let subtype = request
        .get("subtype")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let input = control_request_input(request);

    let response = if is_tool_permission_subtype(subtype) {
        let tool_name = control_request_tool_name(value, request).unwrap_or_default();
        if claude_is_ask_user_question_tool_name(tool_name) {
            return Some(tool_permission_control_response_payload(
                &request_id,
                json!({
                    "behavior": "deny",
                    "message": "Claude AskUserQuestion permission requests must be bridged through Tyde's AskUserQuestion answer flow.",
                }),
            ));
        }
        if claude_is_exit_plan_mode_tool_name(tool_name) {
            return Some(tool_permission_control_response_payload(
                &request_id,
                json!({
                    "behavior": "deny",
                    "message": "Claude ExitPlanMode permission requests must be bridged through Tyde's plan approval flow.",
                }),
            ));
        }
        json!({
            "behavior": "allow",
            "updatedInput": input,
        })
    } else {
        tracing::debug!("Auto-acknowledging Claude control_request subtype={subtype}");
        Value::Null
    };

    Some(tool_permission_control_response_payload(
        &request_id,
        response,
    ))
}

fn tool_permission_control_response_payload(request_id: &str, response: Value) -> Value {
    json!({
        "type": "control_response",
        "response": {
            "subtype": "success",
            "request_id": request_id,
            "response": response,
        },
    })
}

fn is_tool_permission_subtype(subtype: &str) -> bool {
    matches!(
        subtype,
        "can_use_tool" | "canUseTool" | "permission_prompt" | "permissionPrompt"
    )
}

fn ask_user_question_control_request(value: &Value) -> Option<AskUserQuestionControlRequest> {
    if value.get("type").and_then(Value::as_str) != Some("control_request") {
        return None;
    }
    let request = value.get("request").unwrap_or(&Value::Null);
    let subtype = request
        .get("subtype")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !is_tool_permission_subtype(subtype) {
        return None;
    }
    let tool_name = control_request_tool_name(value, request)?;
    if !claude_is_ask_user_question_tool_name(tool_name) {
        return None;
    }
    let request_id = value
        .get("request_id")
        .and_then(Value::as_str)
        .or_else(|| request.get("request_id").and_then(Value::as_str))
        .and_then(normalize_nonempty)?;
    let input = control_request_input(request);
    let tool_call_id = control_request_tool_call_id(value, request).unwrap_or_else(|| {
        format!(
            "claude-ask-user-question-{}",
            normalize_tool_name(&request_id)
        )
    });
    Some(AskUserQuestionControlRequest {
        request_id,
        tool_call_id,
        tool_name: tool_name.to_string(),
        input,
    })
}

fn exit_plan_mode_control_request(value: &Value) -> Option<ExitPlanModeControlRequest> {
    if value.get("type").and_then(Value::as_str) != Some("control_request") {
        return None;
    }
    let request = value.get("request").unwrap_or(&Value::Null);
    let subtype = request
        .get("subtype")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !is_tool_permission_subtype(subtype) {
        return None;
    }
    let tool_name = control_request_tool_name(value, request)?;
    if !claude_is_exit_plan_mode_tool_name(tool_name) {
        return None;
    }
    let request_id = value
        .get("request_id")
        .and_then(Value::as_str)
        .or_else(|| request.get("request_id").and_then(Value::as_str))
        .and_then(normalize_nonempty)?;
    let input = control_request_input(request);
    let tool_call_id = control_request_tool_call_id(value, request)
        .unwrap_or_else(|| format!("claude-exit-plan-mode-{}", normalize_tool_name(&request_id)));
    Some(ExitPlanModeControlRequest {
        request_id,
        tool_call_id,
        tool_name: tool_name.to_string(),
        input,
    })
}

fn control_request_input(request: &Value) -> Value {
    request
        .get("input")
        .or_else(|| request.get("input_data"))
        .or_else(|| request.get("inputData"))
        .or_else(|| request.get("tool_input"))
        .or_else(|| request.get("toolInput"))
        .or_else(|| request.get("tool").and_then(|tool| tool.get("input")))
        .cloned()
        .unwrap_or_else(|| json!({}))
}

fn control_request_tool_name<'a>(value: &'a Value, request: &'a Value) -> Option<&'a str> {
    request
        .get("tool_name")
        .or_else(|| request.get("toolName"))
        .or_else(|| request.get("tool"))
        .or_else(|| request.get("name"))
        .and_then(Value::as_str)
        .or_else(|| {
            request
                .get("tool")
                .and_then(|tool| tool.get("name"))
                .and_then(Value::as_str)
        })
        .or_else(|| value.get("tool_name").and_then(Value::as_str))
        .or_else(|| value.get("toolName").and_then(Value::as_str))
}

fn control_request_tool_call_id(value: &Value, request: &Value) -> Option<String> {
    request
        .get("tool_call_id")
        .or_else(|| request.get("toolCallId"))
        .or_else(|| request.get("tool_use_id"))
        .or_else(|| request.get("toolUseId"))
        .or_else(|| request.get("id"))
        .and_then(Value::as_str)
        .or_else(|| {
            request
                .get("tool")
                .and_then(|tool| {
                    tool.get("id")
                        .or_else(|| tool.get("tool_call_id"))
                        .or_else(|| tool.get("toolCallId"))
                })
                .and_then(Value::as_str)
        })
        .or_else(|| value.get("tool_call_id").and_then(Value::as_str))
        .or_else(|| value.get("toolCallId").and_then(Value::as_str))
        .and_then(normalize_nonempty)
}

fn ask_user_question_control_response_payload(request_id: &str, updated_input: Value) -> Value {
    let answers = updated_input
        .get("answers")
        .cloned()
        .unwrap_or_else(|| json!({}));
    tool_permission_control_response_payload(
        request_id,
        json!({
            "behavior": "allow",
            "updatedInput": updated_input,
            "answers": answers,
        }),
    )
}

fn exit_plan_mode_control_response_payload(
    request_id: &str,
    decision: ExitPlanModeDecision,
    updated_input: Value,
    feedback: &str,
) -> Value {
    let response = match decision {
        ExitPlanModeDecision::Approve => json!({
            "behavior": "allow",
            "updatedInput": updated_input,
        }),
        ExitPlanModeDecision::Reject => json!({
            "behavior": "deny",
            "message": feedback,
        }),
    };
    tool_permission_control_response_payload(request_id, response)
}

fn ask_user_question_input_with_answer(input: &Value, answer: &str) -> Value {
    let mut updated = if input.is_object() {
        input.clone()
    } else {
        json!({ "prompt": input })
    };
    let answers = ask_user_question_answer_map(input, answer);
    if let Some(object) = updated.as_object_mut() {
        object.insert("answers".to_string(), Value::Object(answers));
    }
    updated
}

fn ask_user_question_answer_map(input: &Value, answer: &str) -> serde_json::Map<String, Value> {
    let questions = claude_ask_user_questions(input);
    if questions.is_empty() {
        let mut answers = serde_json::Map::new();
        answers.insert("answer".to_string(), Value::String(answer.to_string()));
        return answers;
    }

    let parsed_lines = parse_ask_user_question_answer_lines(answer);
    questions
        .iter()
        .enumerate()
        .map(|(index, question)| {
            let key = ask_user_question_answer_key(index, question);
            let value = answer_for_ask_user_question(question, answer, &parsed_lines)
                .unwrap_or_else(|| answer.to_string());
            (key, Value::String(value))
        })
        .collect()
}

fn parse_ask_user_question_answer_lines(answer: &str) -> HashMap<String, String> {
    answer
        .lines()
        .filter_map(|line| {
            let (label, value) = line.split_once(':')?;
            let label = label.trim();
            let value = value.trim();
            if label.is_empty() || value.is_empty() {
                None
            } else {
                Some((normalize_tool_name(label), value.to_string()))
            }
        })
        .collect()
}

fn answer_for_ask_user_question(
    question: &protocol::AskUserQuestion,
    fallback: &str,
    parsed_lines: &HashMap<String, String>,
) -> Option<String> {
    let labels = [
        question.id.as_deref(),
        question.header.as_deref(),
        Some(question.question.as_str()),
    ];
    for label in labels.into_iter().flatten() {
        if let Some(answer) = parsed_lines.get(&normalize_tool_name(label)) {
            return Some(answer.clone());
        }
    }
    if parsed_lines.is_empty() {
        Some(fallback.to_string())
    } else {
        None
    }
}

fn ask_user_question_answer_key(index: usize, question: &protocol::AskUserQuestion) -> String {
    if let Some(question_text) = normalize_nonempty(&question.question) {
        return question_text;
    }
    question
        .header
        .as_deref()
        .and_then(normalize_nonempty)
        .or_else(|| question.id.as_deref().and_then(normalize_nonempty))
        .unwrap_or_else(|| format!("question_{}", index + 1))
}

async fn fail_pending_control_waiters(control_waiters: &ClaudeControlWaiters, message: &str) {
    let waiters = {
        let mut guard = control_waiters.lock().await;
        std::mem::take(&mut *guard)
    };
    for (_request_id, waiter) in waiters {
        let _ = waiter.send(Err(message.to_string()));
    }
}

async fn read_claude_stderr_persistent(stderr: ChildStderr, inner: Arc<ClaudeInner>) {
    let mut lines = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        tracing::debug!("Claude stderr: {line}");
        inner.emitter.subprocess_stderr(&line);
    }
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
    maybe_emit_subagent_progress(stream);
}

/// Minimum interval between live-status updates on the parent's Task
/// tool card while routing a sub-agent's events.
const SUBAGENT_PROGRESS_EMIT_INTERVAL: Duration = Duration::from_millis(250);

fn subagent_progress_data(stream: &SubAgentStream, completed: bool) -> ToolProgressData {
    ToolProgressData {
        tool_call_id: stream.parent_tool_use_id.clone(),
        tool_name: "Task".to_string(),
        update: ToolProgressUpdate::SubAgent(protocol::SubAgentProgress {
            agent_id: stream.agent_id.clone(),
            agent_name: stream.agent_name.clone(),
            last_tool_name: stream
                .summary
                .tool_calls
                .last()
                .map(|tool| tool.name.clone()),
            tool_calls: stream.summary.tool_calls.len() as u64,
            completed,
        }),
    }
}

fn maybe_emit_subagent_progress(stream: &mut SubAgentStream) {
    if stream.last_progress_emit.elapsed() < SUBAGENT_PROGRESS_EMIT_INTERVAL {
        return;
    }
    stream.last_progress_emit = std::time::Instant::now();
    stream
        .parent_emitter
        .tool_progress(&subagent_progress_data(stream, false));
}

fn emit_subagent_task_prompt_if_needed(stream: &mut SubAgentStream, description: &str) {
    let trimmed = description.trim();
    if stream.has_explicit_task_prompt || trimmed.is_empty() {
        return;
    }
    stream.has_explicit_task_prompt = true;
    stream.inner.emitter.user_message(trimmed, Vec::new());
}

struct SubAgentSpawnSpec {
    tool_use_id: String,
    name: String,
    description: String,
    agent_type: String,
    session_id_hint: Option<protocol::SessionId>,
    execution: SubAgentExecution,
}

async fn ensure_subagent_stream(
    emitter: &dyn SubAgentEmitter,
    parent_emitter: &Arc<TurnEmitter>,
    streams: &mut HashMap<String, SubAgentStream>,
    spec: SubAgentSpawnSpec,
) {
    let SubAgentSpawnSpec {
        tool_use_id,
        name,
        description,
        agent_type,
        session_id_hint,
        execution,
    } = spec;
    if streams.contains_key(&tool_use_id) {
        return;
    }

    tracing::info!(
        "registering Claude sub-agent stream tool_use_id={tool_use_id} name={name} agent_type={agent_type}"
    );
    let handle = match emitter
        .on_subagent_spawned(
            tool_use_id.clone(),
            name.clone(),
            description,
            agent_type,
            session_id_hint,
        )
        .await
    {
        Ok(handle) => handle,
        Err(error) => {
            parent_emitter.backend_error(&format!(
                "Claude child relay registration failed for tool '{}': {error}",
                tool_use_id
            ));
            return;
        }
    };
    let (raw_event_tx, raw_event_rx) = mpsc::unbounded_channel();
    spawn_claude_subagent_event_bridge(raw_event_rx, handle.event_tx.clone());

    // Create a ClaudeInner that routes events to the sub-agent's channel.
    let sa_inner = Arc::new(ClaudeInner {
        emitter: Arc::new(TurnEmitter::new_for_agent(
            raw_event_tx,
            AgentName(CLAUDE_AGENT_NAME),
        )),
        state: Mutex::new(ClaudeState::default()),
        runtime: Mutex::new(None),
        turn_event_gate: Mutex::new(()),
    });
    let sa_message_id = format!("subagent-{}", tool_use_id);

    let stream = SubAgentStream {
        summary: ClaudeStdoutSummary::default(),
        segment: SegmentState {
            awaiting_stream_start: true,
            ..SegmentState::default()
        },
        message_id: sa_message_id,
        has_explicit_task_prompt: false,
        inner: sa_inner,
        parent_tool_use_id: tool_use_id.clone(),
        agent_id: handle.agent_id,
        agent_name: name,
        parent_emitter: parent_emitter.clone(),
        last_progress_emit: std::time::Instant::now(),
        execution,
    };
    // Unthrottled spawn update: the Task card learns the sub-agent's id
    // (for its "Open agent" link) as soon as the agent exists.
    parent_emitter.tool_progress(&subagent_progress_data(&stream, false));
    streams.insert(tool_use_id, stream);
}

// ============================================================================
// Workflow task frames → live ToolProgress snapshots.
//
// The Claude CLI runs the Workflow tool as a background task: the tool
// call returns a run id within seconds, then `system` frames
// (`task_started` / `task_progress` / `task_notification`) keep flowing —
// mostly *after* the tool result and across turn boundaries. The
// `workflow_progress` array on `task_progress` frames carries per-agent
// *delta* events; this reducer folds them into a full `WorkflowRunState`
// snapshot and emits it as `ToolProgress` on the parent emitter, keyed by
// the Workflow tool call's `tool_use_id`.
// ============================================================================

/// Minimum interval between emitted snapshots per run. State transitions
/// (an agent starting/finishing, the run completing) always flush
/// immediately so short workflows never render stale.
const WORKFLOW_PROGRESS_EMIT_INTERVAL: Duration = Duration::from_millis(500);

struct WorkflowRunEntry {
    tool_use_id: String,
    state: WorkflowRunState,
    last_emit: std::time::Instant,
}

fn map_workflow_agent_status(raw: &str) -> WorkflowAgentStatus {
    match raw {
        "queued" => WorkflowAgentStatus::Queued,
        "start" | "running" | "progress" => WorkflowAgentStatus::Running,
        "done" => WorkflowAgentStatus::Done,
        "error" | "failed" => WorkflowAgentStatus::Error,
        _ => WorkflowAgentStatus::Unknown,
    }
}

/// Fold one `workflow_progress` delta into the run state. Returns `true`
/// when the delta changed an agent's status (a transition worth flushing
/// immediately). Entry types other than `workflow_agent` (workflow-level
/// records) are not consumed by this reducer.
fn apply_workflow_agent_delta(
    state: &mut WorkflowRunState,
    delta: &ClaudeWorkflowAgentDelta,
) -> bool {
    if delta.kind != "workflow_agent" {
        return false;
    }
    let Some(index) = delta.index else {
        tracing::warn!("workflow_agent delta without index: {delta:?}");
        return false;
    };

    let position = match state
        .agents
        .binary_search_by_key(&index, |agent| agent.index)
    {
        Ok(position) => position,
        Err(position) => {
            state.agents.insert(
                position,
                WorkflowAgentState {
                    index,
                    label: String::new(),
                    phase_title: None,
                    model: None,
                    state: WorkflowAgentStatus::Queued,
                    tokens: 0,
                    tool_calls: 0,
                    duration_ms: 0,
                    attempt: 1,
                    prompt_preview: None,
                    result_preview: None,
                },
            );
            position
        }
    };
    let agent = &mut state.agents[position];

    if let Some(label) = &delta.label {
        agent.label = label.clone();
    }
    if let Some(phase) = &delta.phase_title {
        agent.phase_title = Some(phase.clone());
    }
    if let Some(model) = &delta.model {
        agent.model = Some(model.clone());
    }
    if let Some(attempt) = delta.attempt {
        agent.attempt = attempt;
    }
    if let Some(tokens) = delta.tokens {
        agent.tokens = tokens;
    }
    if let Some(tool_calls) = delta.tool_calls {
        agent.tool_calls = tool_calls;
    }
    if let Some(duration_ms) = delta.duration_ms {
        agent.duration_ms = duration_ms;
    }
    if let Some(preview) = &delta.prompt_preview {
        agent.prompt_preview = Some(preview.clone());
    }
    if let Some(preview) = &delta.result_preview {
        agent.result_preview = Some(preview.clone());
    }

    let mut transitioned = false;
    if let Some(raw_status) = &delta.state {
        let status = map_workflow_agent_status(raw_status);
        if status == WorkflowAgentStatus::Unknown {
            tracing::warn!("unknown workflow agent state '{raw_status}' (agent {index})");
        }
        if agent.state != status {
            agent.state = status;
            transitioned = true;
        }
    }
    transitioned
}

fn apply_workflow_usage(state: &mut WorkflowRunState, usage: &ClaudeTaskUsage) {
    if let Some(total_tokens) = usage.total_tokens {
        state.total_tokens = total_tokens;
    }
    if let Some(tool_uses) = usage.tool_uses {
        state.tool_uses = tool_uses;
    }
    if let Some(duration_ms) = usage.duration_ms {
        state.duration_ms = duration_ms;
    }
}

fn emit_workflow_snapshot(emitter: &TurnEmitter, entry: &mut WorkflowRunEntry) {
    entry.last_emit = std::time::Instant::now();
    emitter.tool_progress(&ToolProgressData {
        tool_call_id: entry.tool_use_id.clone(),
        tool_name: "Workflow".to_string(),
        update: ToolProgressUpdate::Workflow(entry.state.clone()),
    });
}

/// Consume a workflow task frame if `value` is one. Returns `true` when
/// the frame was handled (the caller skips all per-turn processing —
/// these frames arrive between turns too, where the per-turn path would
/// drop them).
fn handle_workflow_task_frame(
    value: &Value,
    workflow_runs: &mut HashMap<String, WorkflowRunEntry>,
    emitter: &TurnEmitter,
) -> bool {
    if value.get("type").and_then(Value::as_str) != Some("system") {
        return false;
    }
    // Parse failures fall through to `consume_claude_stream_value`, which
    // warns about any unparseable system frame.
    let Ok(system) = parse_claude_system_frame(value) else {
        return false;
    };
    let Some(task_id) = system.task_id.as_deref().and_then(normalize_nonempty) else {
        return false;
    };

    match system.event() {
        ClaudeSystemEvent::TaskStarted => {
            if system.task_type.as_deref() != Some("local_workflow") {
                return false;
            }
            let Some(tool_use_id) = system.tool_use_id.as_deref().and_then(normalize_nonempty)
            else {
                tracing::warn!("ignoring workflow task_started without tool_use_id: {value}");
                return true;
            };
            // No fallback name: the CLI sends `workflow_name` on every
            // workflow task_started. If it ever doesn't, surface that
            // instead of inventing a label.
            let Some(workflow_name) = system.workflow_name.as_deref().and_then(normalize_nonempty)
            else {
                tracing::warn!("ignoring workflow task_started without workflow_name: {value}");
                return true;
            };
            let mut entry = WorkflowRunEntry {
                tool_use_id,
                state: WorkflowRunState {
                    workflow_name,
                    description: system.description.as_deref().and_then(normalize_nonempty),
                    script: system.prompt.as_deref().and_then(normalize_nonempty),
                    status: WorkflowRunStatus::Running,
                    summary: None,
                    total_tokens: 0,
                    tool_uses: 0,
                    duration_ms: 0,
                    agents: Vec::new(),
                },
                last_emit: std::time::Instant::now(),
            };
            emit_workflow_snapshot(emitter, &mut entry);
            workflow_runs.insert(task_id, entry);
            true
        }
        ClaudeSystemEvent::TaskProgress => {
            let Some(entry) = workflow_runs.get_mut(&task_id) else {
                // Not a workflow task (e.g. a local_agent task) — let the
                // regular paths see the frame.
                return false;
            };
            let mut transitioned = false;
            for raw_delta in system.workflow_progress.iter().flatten() {
                match serde_json::from_value::<ClaudeWorkflowAgentDelta>(raw_delta.clone()) {
                    Ok(delta) => {
                        transitioned |= apply_workflow_agent_delta(&mut entry.state, &delta);
                    }
                    Err(err) => {
                        tracing::warn!(
                            "skipping malformed workflow_progress delta: {err}; value={raw_delta}"
                        );
                    }
                }
            }
            if let Some(usage) = system.usage.as_ref() {
                apply_workflow_usage(&mut entry.state, usage);
            }
            if transitioned || entry.last_emit.elapsed() >= WORKFLOW_PROGRESS_EMIT_INTERVAL {
                emit_workflow_snapshot(emitter, entry);
            }
            true
        }
        ClaudeSystemEvent::TaskNotification => {
            let Some(mut entry) = workflow_runs.remove(&task_id) else {
                return false;
            };
            entry.state.status = match system.status.as_deref() {
                Some("completed") => WorkflowRunStatus::Completed,
                Some("failed") | Some("error") => WorkflowRunStatus::Failed,
                other => {
                    tracing::warn!("unknown workflow task_notification status: {other:?}");
                    WorkflowRunStatus::Unknown
                }
            };
            entry.state.summary = system.summary.as_deref().and_then(normalize_nonempty);
            emit_workflow_snapshot(emitter, &mut entry);
            true
        }
        _ => false,
    }
}

async fn detect_subagent_task_system_spawns(
    value: &Value,
    emitter: &dyn SubAgentEmitter,
    parent_emitter: &Arc<TurnEmitter>,
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
        parent_emitter,
        streams,
        SubAgentSpawnSpec {
            tool_use_id: tool_use_id.clone(),
            name,
            description,
            agent_type: task_type,
            session_id_hint: None,
            execution: SubAgentExecution::Unknown,
        },
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
    parent_emitter: &Arc<TurnEmitter>,
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
            let requested_execution = extract_run_in_background(&block).map(|background| {
                if background {
                    SubAgentExecution::Background
                } else {
                    SubAgentExecution::Foreground
                }
            });
            ensure_subagent_stream(
                emitter,
                parent_emitter,
                streams,
                SubAgentSpawnSpec {
                    tool_use_id: tool_use_id.clone(),
                    name,
                    description: description.clone(),
                    agent_type,
                    session_id_hint: None,
                    execution: requested_execution.unwrap_or_default(),
                },
            )
            .await;
            if let Some(stream) = streams.get_mut(&tool_use_id) {
                if let Some(execution) = requested_execution {
                    stream.execution = execution;
                }
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
        // A background sub-agent's tool_result is the synthetic "launched"
        // placeholder — its real output streams *afterwards*. Keep the stream
        // alive; it is finalized on the `task_notification` completion frame
        // (see `finalize_background_subagent_completion`).
        match streams.get(tool_use_id).map(|stream| stream.execution) {
            Some(SubAgentExecution::Background | SubAgentExecution::Unknown) => continue,
            Some(SubAgentExecution::Foreground) | None => {}
        }
        if let Some(stream) = streams.remove(tool_use_id) {
            finalize_subagent_stream(stream);
        }
    }
}

/// Flush and close out a sub-agent stream, emitting its final progress stats.
fn finalize_subagent_stream(mut stream: SubAgentStream) {
    flush_pending_tool_uses(&mut stream.summary, &mut stream.segment);
    if phase_has_pending_output(&stream.summary, &stream.segment) {
        close_current_subagent_phase(&mut stream.summary, &mut stream.segment, &stream.inner);
    } else if let Some(turn_usage) = subagent_terminal_usage(&stream.summary) {
        let base_message_id = stream.message_id.clone();
        let mut terminal_message_id = base_message_id.clone();
        let model = stream.summary.model.clone();
        maybe_emit_next_stream_start(
            &mut stream.summary,
            &mut stream.segment,
            &stream.inner,
            &base_message_id,
            &mut terminal_message_id,
            model,
        );
        stream.message_id = terminal_message_id;
        stream.inner.emit_placeholder_stream_end(
            stream.summary.model.clone(),
            Some(turn_usage),
            None,
        );
    }
    // Unthrottled final update with the closing stats.
    stream
        .parent_emitter
        .tool_progress(&subagent_progress_data(&stream, true));
}

fn close_current_subagent_phase(
    summary: &mut ClaudeStdoutSummary,
    segment: &mut SegmentState,
    inner: &ClaudeInner,
) {
    let turn_usage = subagent_terminal_usage(summary);
    close_current_phase_with_turn_usage(summary, segment, inner, turn_usage);
}

fn subagent_terminal_usage(summary: &ClaudeStdoutSummary) -> Option<ClaudeTurnUsage> {
    summary
        .result_turn_usage
        .clone()
        .or_else(|| {
            summary
                .usage
                .as_ref()
                .map(|usage| add_token_usage(summary.accumulated_request_usage.as_ref(), usage))
        })
        .or_else(|| summary.accumulated_request_usage.clone())
        .map(|usage| ClaudeTurnUsage {
            turn: usage.clone(),
            cumulative: Some(usage),
        })
}

/// Finalize a background sub-agent when its `task_notification` completion
/// frame arrives. These frames flow on the parent stream (no
/// `parent_tool_use_id`) and keep coming after the parent's turn `result`,
/// so this runs pre-gate in `read_claude_stdout_persistent`.
fn finalize_background_subagent_completion(
    value: &Value,
    streams: &mut HashMap<String, SubAgentStream>,
) {
    if value.get("type").and_then(Value::as_str) != Some("system") {
        return;
    }
    if value.get("subtype").and_then(Value::as_str) != Some("task_notification") {
        return;
    }
    let Some(tool_use_id) = value
        .get("tool_use_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    else {
        return;
    };
    if let Some(stream) = streams.remove(tool_use_id) {
        finalize_subagent_stream(stream);
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
            inner.emitter.session_started(session_id);
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
            // Never panic on CLI output — the system-frame format is
            // unversioned and grows new fields/subtypes over time.
            let system = match parse_claude_system_frame(value) {
                Ok(system) => system,
                Err(err) => {
                    tracing::warn!("Ignoring unparseable Claude system frame: {err}");
                    return;
                }
            };
            if let Some(model) = system.model.as_ref() {
                summary.model = Some(model.clone());
            }
            match system.event() {
                ClaudeSystemEvent::Init => {}
                ClaudeSystemEvent::Status => {}
                ClaudeSystemEvent::CompactBoundary => {
                    summary.control_event = Some(ClaudeControlEvent::ConversationCompacted);
                }
                // Workflow task frames are consumed pre-gate in
                // `read_claude_stdout_persistent` (they keep arriving
                // between turns, when this per-turn path never runs);
                // anything reaching here is a non-workflow task event
                // with nothing to render.
                ClaudeSystemEvent::TaskStarted
                | ClaudeSystemEvent::TaskProgress
                | ClaudeSystemEvent::TaskNotification
                | ClaudeSystemEvent::BackgroundTasksChanged
                | ClaudeSystemEvent::TaskUpdated => {
                    let _ = (&system.task_id, &system.status, &system.summary);
                }
                ClaudeSystemEvent::ThinkingTokens | ClaudeSystemEvent::ApiRetry => {}
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
            // result.usage aggregates the API calls made by this CLI invocation.
            // Store it separately from the latest per-call assistant usage.
            if let Some(usage) = parse_token_usage(value.get("usage")) {
                summary.result_turn_usage = Some(usage);
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
            summary,
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
        if summary
            .auto_closed_tool_requests
            .contains(&completion.tool_call_id)
        {
            tracing::debug!(
                tool_call_id = completion.tool_call_id,
                "skipping Claude tool completion after synthetic auto-close"
            );
            continue;
        }
        if summary
            .unresolved_tool_requests
            .remove(&completion.tool_call_id)
            .is_none()
        {
            tracing::debug!(
                tool_call_id = completion.tool_call_id,
                "skipping Claude tool completion without emitted ToolRequest"
            );
            continue;
        }
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
    summary: &mut ClaudeStdoutSummary,
    segment: &mut SegmentState,
    inner: &ClaudeInner,
    base_message_id: &str,
    current_message_id: &mut String,
    model: Option<String>,
) {
    if !segment.awaiting_stream_start {
        return;
    }

    auto_close_unresolved_tool_requests(
        summary,
        inner,
        "Claude started a new assistant response before returning a result for this streamed tool request.",
    );
    segment.segment_index += 1;
    *current_message_id = format!("{base_message_id}-seg-{}", segment.segment_index);
    inner.emit_stream_start(current_message_id, model);
    segment.awaiting_stream_start = false;
}

fn phase_usage_for_emission(summary: &mut ClaudeStdoutSummary) -> Option<Value> {
    summary.usage.take()
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
    close_current_phase_with_turn_usage(summary, segment, inner, None);
}

fn close_current_phase_with_turn_usage(
    summary: &mut ClaudeStdoutSummary,
    segment: &mut SegmentState,
    inner: &ClaudeInner,
    turn_usage: Option<ClaudeTurnUsage>,
) {
    flush_pending_tool_uses(summary, segment);
    enrich_exit_plan_mode_tool_calls(summary);

    if let Some(phase) = take_phase_emission(summary) {
        if let Some(request_usage) = phase.usage.as_ref() {
            summary.accumulated_request_usage = Some(add_token_usage(
                summary.accumulated_request_usage.as_ref(),
                request_usage,
            ));
        }
        let turn = turn_usage.as_ref().map(|usage| usage.turn.clone());
        let cumulative = turn_usage.and_then(|usage| usage.cumulative);
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
            ClaudeMessageUsage {
                request: phase.usage,
                turn,
                cumulative,
            },
            phase.reasoning,
            tool_calls,
            None,
        );
        for tool_call in &phase.tool_calls {
            emit_tool_request_with_tracking(summary, inner, tool_call);
        }
    }

    reset_phase_state(summary, segment);
    segment.awaiting_stream_start = true;
}

fn emit_tool_request_with_tracking(
    summary: &mut ClaudeStdoutSummary,
    inner: &ClaudeInner,
    tool_call: &ClaudeToolCall,
) {
    inner.emit_tool_request(tool_call);
    summary
        .unresolved_tool_requests
        .insert(tool_call.id.clone(), tool_call.name.clone());
}

fn auto_close_unresolved_tool_requests(
    summary: &mut ClaudeStdoutSummary,
    inner: &ClaudeInner,
    message: &str,
) {
    let unresolved = std::mem::take(&mut summary.unresolved_tool_requests);
    for (tool_call_id, tool_name) in unresolved {
        summary
            .auto_closed_tool_requests
            .insert(tool_call_id.clone());
        inner.emit_tool_execution_completed(
            &tool_call_id,
            &tool_name,
            false,
            json!({
                "kind": "Error",
                "short_message": "Tool result missing",
                "detailed_message": message,
            }),
            Some(message.to_string()),
        );
    }
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
                summary,
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
                        summary,
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
                        summary,
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
                    summary,
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
                            summary,
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
                            summary,
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
                    summary,
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

fn add_token_usage(accumulated: Option<&Value>, usage: &Value) -> Value {
    let context_window = usage
        .get("context_window")
        .and_then(Value::as_u64)
        .or_else(|| {
            accumulated.and_then(|value| value.get("context_window").and_then(Value::as_u64))
        });
    let summed = |key| {
        accumulated
            .map(|value| usage_value_u64(value, key))
            .unwrap_or(0)
            .saturating_add(usage_value_u64(usage, key))
    };

    json!({
        "input_tokens": summed("input_tokens"),
        "output_tokens": summed("output_tokens"),
        "total_tokens": summed("total_tokens"),
        "cached_prompt_tokens": summed("cached_prompt_tokens"),
        "cache_creation_input_tokens": summed("cache_creation_input_tokens"),
        "reasoning_tokens": summed("reasoning_tokens"),
        "context_window": context_window,
    })
}

#[cfg(test)]
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

fn parse_claude_effort_setting(value: &Value) -> Result<Option<ClaudeEffort>, String> {
    if value.is_null() {
        return Ok(None);
    }
    let text = value
        .as_str()
        .ok_or_else(|| format!("Claude effort must be a string or null, got {value}"))?;
    normalize_nonempty(text)
        .map(|text| ClaudeEffort::parse(&text))
        .transpose()
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
    let mut content_blocks = Vec::new();
    if !prompt.trim().is_empty() {
        content_blocks.push(json!({
            "type": "text",
            "text": prompt,
        }));
    }

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

fn claude_is_ask_user_question_tool_name(tool_name: &str) -> bool {
    normalize_tool_name(tool_name) == "askuserquestion"
}

fn claude_is_exit_plan_mode_tool_name(tool_name: &str) -> bool {
    normalize_tool_name(tool_name) == "exitplanmode"
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
/// Build a `TaskList` payload from a Claude `TodoWrite` tool call's
/// `arguments`. Returns `None` when the call does not carry a todos
/// array. Emission goes through `emitter.task_update`; callers must
/// deserialize into `protocol::TaskList` before passing on.
fn claude_task_update_from_todo_write(arguments: &Value) -> Option<protocol::TaskList> {
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
    let value = json!({
        "title": "",
        "tasks": tasks,
    });
    serde_json::from_value::<protocol::TaskList>(value).ok()
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

#[derive(Clone, Default)]
struct ExitPlanModePlanInfo {
    plan: Option<String>,
    plan_path: Option<String>,
}

fn exit_plan_mode_plan_info_from_arguments(arguments: &Value) -> ExitPlanModePlanInfo {
    ExitPlanModePlanInfo {
        plan: claude_argument_string(
            arguments,
            &["plan", "plan_content", "planContent", "content"],
        ),
        plan_path: claude_argument_string(
            arguments,
            &[
                "plan_path",
                "planPath",
                "planFilePath",
                "file_path",
                "filePath",
                "path",
            ],
        ),
    }
}

fn exit_plan_mode_plan_info_from_tool_calls<'a>(
    tool_calls: impl IntoIterator<Item = &'a ClaudeToolCall>,
) -> Option<ExitPlanModePlanInfo> {
    tool_calls.into_iter().find_map(|tool_call| {
        if normalize_tool_name(&tool_call.name) != "write" {
            return None;
        }
        let plan_path = claude_argument_file_path(&tool_call.arguments)?;
        if !plan_path.contains(".claude/plans/") {
            return None;
        }
        let plan =
            claude_argument_string(&tool_call.arguments, &["content", "text", "new_content"])?;
        Some(ExitPlanModePlanInfo {
            plan: Some(plan),
            plan_path: Some(plan_path),
        })
    })
}

fn enrich_exit_plan_mode_arguments(
    arguments: Value,
    fallback: Option<ExitPlanModePlanInfo>,
) -> Value {
    let mut object = arguments.as_object().cloned().unwrap_or_default();
    let existing = exit_plan_mode_plan_info_from_arguments(&Value::Object(object.clone()));
    if existing.plan.is_none()
        && let Some(plan) = fallback.as_ref().and_then(|info| info.plan.clone())
    {
        object.insert("plan".to_string(), Value::String(plan));
    }
    if existing.plan_path.is_none()
        && let Some(plan_path) = fallback.as_ref().and_then(|info| info.plan_path.clone())
    {
        object.insert("planFilePath".to_string(), Value::String(plan_path));
    }
    Value::Object(object)
}

fn enrich_exit_plan_mode_tool_calls(summary: &mut ClaudeStdoutSummary) {
    let Some(plan_info) = exit_plan_mode_plan_info_from_tool_calls(summary.tool_calls.iter())
        .or_else(|| exit_plan_mode_plan_info_from_tool_calls(summary.tool_call_by_id.values()))
    else {
        return;
    };

    let mut changed = Vec::new();
    for tool_call in &mut summary.tool_calls {
        if !claude_is_exit_plan_mode_tool_name(&tool_call.name) {
            continue;
        }
        let enriched =
            enrich_exit_plan_mode_arguments(tool_call.arguments.clone(), Some(plan_info.clone()));
        if enriched != tool_call.arguments {
            tool_call.arguments = enriched;
            changed.push(tool_call.clone());
        }
    }
    for tool_call in changed {
        summary
            .tool_call_by_id
            .insert(tool_call.id.clone(), tool_call);
    }
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

fn claude_ask_user_questions(arguments: &Value) -> Vec<protocol::AskUserQuestion> {
    if let Some(questions) = arguments.get("questions").and_then(Value::as_array) {
        return questions
            .iter()
            .map(claude_ask_user_question_from_value)
            .collect();
    }

    if arguments
        .as_object()
        .is_some_and(|object| !object.is_empty())
    {
        return vec![claude_ask_user_question_from_value(arguments)];
    }

    Vec::new()
}

fn claude_ask_user_question_from_value(value: &Value) -> protocol::AskUserQuestion {
    protocol::AskUserQuestion {
        id: claude_argument_string(value, &["id"]),
        question: claude_argument_string(value, &["question", "prompt"]).unwrap_or_default(),
        header: claude_argument_string(value, &["header", "title"]),
        options: claude_ask_user_question_options(value),
        multi_select: claude_argument_bool(value, &["multiSelect", "multi_select"])
            .unwrap_or(false),
    }
}

fn claude_ask_user_question_options(value: &Value) -> Vec<protocol::AskUserQuestionOption> {
    let Some(options) = value.get("options").and_then(Value::as_array) else {
        return Vec::new();
    };

    options
        .iter()
        .map(|option| {
            if let Some(label) = option.as_str().and_then(normalize_nonempty) {
                return protocol::AskUserQuestionOption {
                    label,
                    description: None,
                };
            }

            protocol::AskUserQuestionOption {
                label: claude_argument_string(option, &["label", "value"]).unwrap_or_default(),
                description: claude_argument_string(option, &["description"]),
            }
        })
        .collect()
}

fn claude_argument_bool(arguments: &Value, keys: &[&str]) -> Option<bool> {
    for key in keys {
        if let Some(value) = arguments.get(*key).and_then(Value::as_bool) {
            return Some(value);
        }
    }
    None
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

    if claude_is_ask_user_question_tool_name(tool_name) {
        return json!({
            "kind": "AskUserQuestion",
            "questions": claude_ask_user_questions(arguments),
        });
    }

    if claude_is_exit_plan_mode_tool_name(tool_name) {
        let plan_info = exit_plan_mode_plan_info_from_arguments(arguments);
        return json!({
            "kind": "ExitPlanMode",
            "plan": plan_info.plan,
            "plan_path": plan_info.plan_path,
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
    // Matches Claude CLI's project-directory naming: it replaces path separators,
    // the drive-letter colon, dots, and underscores with '-' so that any filesystem
    // path collapses into a single flat directory name under ~/.claude/projects/.
    // Missing `_` caused macOS temp-dir paths like
    // /var/folders/<dir>/29t_skrx.../T/tmp.XXX to encode differently than Claude's
    // own path, so --resume pointed at a path that didn't exist.
    trimmed
        .chars()
        .map(|ch| {
            if ch == '/' || ch == '\\' || ch == ':' || ch == '.' || ch == '_' {
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
) -> Result<ClaudeSessionReplay, ClaudeSessionHistoryError> {
    let session_file = claude_session_file_path(workspace_root, session_id)
        .map_err(ClaudeSessionHistoryError::other)?;
    match tokio_fs::metadata(&session_file).await {
        Ok(metadata) if metadata.is_file() => {}
        Ok(_) => {
            return Err(ClaudeSessionHistoryError::other(format!(
                "Claude session '{}' is not a file",
                session_file.display()
            )));
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(ClaudeSessionHistoryError::missing(
                session_file.display().to_string(),
                err.to_string(),
            ));
        }
        Err(err) => {
            return Err(ClaudeSessionHistoryError::other(format!(
                "Failed to inspect Claude session '{}' for resume: {err}",
                session_file.display()
            )));
        }
    }

    let mut last_err = None;
    for attempt in 0..20 {
        match tokio_fs::read_to_string(&session_file).await {
            Ok(contents) => return Ok(parse_claude_session_replay(&contents)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound && attempt < 19 => {
                last_err = Some(err);
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            Err(err) => {
                return Err(ClaudeSessionHistoryError::other(format!(
                    "Failed to read Claude session '{}' for resume: {err}",
                    session_file.display()
                )));
            }
        }
    }

    let err = last_err.unwrap_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "Claude session file did not appear in time",
        )
    });
    Err(ClaudeSessionHistoryError::missing(
        session_file.display().to_string(),
        err.to_string(),
    ))
}

#[cfg(test)]
fn parse_claude_session_history_contents(contents: &str) -> Vec<ClaudeHistoryReplayItem> {
    parse_claude_session_replay(contents).items
}

fn parse_claude_session_replay(contents: &str) -> ClaudeSessionReplay {
    let mut restored = Vec::new();
    let mut cumulative_usage = None;
    let mut cumulative_usage_complete = true;
    let mut invocation_usage = None;
    let mut invocation_usage_complete = true;
    let mut invocation_message_ids = HashSet::new();
    let mut invocation_prompt_id = None::<String>;
    let mut conversation_bytes_total = 0u64;
    let parsed_values = contents
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                return None;
            }
            serde_json::from_str::<Value>(trimmed).ok()
        })
        .collect::<Vec<_>>();

    let mut tool_name_by_id = HashMap::<String, String>::new();
    let mut tool_call_by_id = HashMap::<String, ClaudeToolCall>::new();
    for value in &parsed_values {
        if value.get("type").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let Some(message) = value.get("message").and_then(Value::as_object) else {
            continue;
        };
        if message.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }

        let message_value = Value::Object(message.clone());
        for tool_call in extract_tool_calls_from_message(&message_value) {
            let tool_call_id = tool_call.id.clone();
            tool_name_by_id.insert(tool_call_id.clone(), tool_call.name.clone());
            tool_call_by_id.insert(tool_call_id, tool_call);
        }
    }

    let mut emitted_tool_requests = HashSet::<String>::new();
    let mut pending_tool_requests = HashMap::<String, ClaudeToolCall>::new();
    let mut auto_closed_tool_requests = HashSet::<String>::new();
    let mut deferred_completions = Vec::<ClaudeReplayToolExecution>::new();
    let mut last_emitted_assistant_message_id = None::<String>;

    for value in parsed_values {
        let line_type = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if line_type == "user"
            && let Some(prompt_id) = replay_top_level_user_prompt_id(&value)
            && invocation_prompt_id.as_deref() != Some(prompt_id.as_str())
        {
            if invocation_prompt_id.is_some() {
                commit_replay_invocation_usage(
                    &mut cumulative_usage,
                    &mut cumulative_usage_complete,
                    &mut invocation_usage,
                    &mut invocation_usage_complete,
                    &mut invocation_message_ids,
                );
            }
            invocation_prompt_id = Some(prompt_id);
        }
        if line_type == "result" {
            if let Some(usage) = parse_token_usage(value.get("usage")) {
                cumulative_usage = Some(add_token_usage(cumulative_usage.as_ref(), &usage));
                invocation_usage = None;
                invocation_usage_complete = true;
                invocation_message_ids.clear();
            } else {
                commit_replay_invocation_usage(
                    &mut cumulative_usage,
                    &mut cumulative_usage_complete,
                    &mut invocation_usage,
                    &mut invocation_usage_complete,
                    &mut invocation_message_ids,
                );
            }
            invocation_prompt_id = None;
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
        if role == "assistant"
            && let Some(usage) = token_usage.as_ref()
        {
            let usage_id = message
                .get("id")
                .and_then(Value::as_str)
                .and_then(normalize_nonempty);
            if let Some(usage_id) = usage_id {
                if invocation_message_ids.insert(usage_id) {
                    invocation_usage = Some(add_token_usage(invocation_usage.as_ref(), usage));
                }
            } else {
                invocation_usage_complete = false;
            }
        }
        let tool_calls = if role == "assistant" {
            extract_tool_calls_from_message(&message_value)
        } else {
            Vec::new()
        };
        let message_tool_calls: Vec<Value> = tool_calls
            .iter()
            .map(|tool_call| {
                json!({
                    "id": tool_call.id.clone(),
                    "name": tool_call.name.clone(),
                    "arguments": tool_call.arguments.clone(),
                })
            })
            .collect();
        let assistant_message_id = if role == "assistant" {
            message
                .get("id")
                .and_then(Value::as_str)
                .and_then(normalize_nonempty)
        } else {
            None
        };
        let same_assistant_message = role == "assistant"
            && assistant_message_id.is_some()
            && assistant_message_id == last_emitted_assistant_message_id;
        // Claude can write one assistant response as multiple JSONL rows with
        // the same message id, especially one pure tool_use row per tool. The
        // frontend protocol treats those as one assistant turn: additional tool
        // requests may arrive while earlier requests from the same turn are
        // pending, but a second assistant MessageAdded may not.
        let has_assistant_message_content = !text.trim().is_empty()
            || !images.is_empty()
            || !message_tool_calls.is_empty()
            || reasoning_text
                .as_ref()
                .is_some_and(|value| !value.trim().is_empty());
        let is_tool_only_assistant_continuation = same_assistant_message
            && text.trim().is_empty()
            && images.is_empty()
            && reasoning_text
                .as_ref()
                .is_none_or(|value| value.trim().is_empty())
            && !message_tool_calls.is_empty();

        let should_emit_message = if role == "assistant" {
            has_assistant_message_content && !is_tool_only_assistant_continuation
        } else {
            !text.trim().is_empty() || !images.is_empty()
        };

        if should_emit_message {
            flush_unresolved_replay_tool_requests(
                &mut restored,
                &mut pending_tool_requests,
                &mut auto_closed_tool_requests,
            );
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
            if role == "assistant" {
                last_emitted_assistant_message_id = assistant_message_id;
            } else {
                last_emitted_assistant_message_id = None;
            }
        }

        if role == "assistant" {
            let current_tool_call_ids = tool_calls
                .iter()
                .map(|tool_call| tool_call.id.clone())
                .collect::<HashSet<_>>();
            for tool_call in tool_calls {
                emitted_tool_requests.insert(tool_call.id.clone());
                pending_tool_requests.insert(tool_call.id.clone(), tool_call.clone());
                restored.push(ClaudeHistoryReplayItem::ToolRequest(tool_call));
            }
            if !current_tool_call_ids.is_empty() {
                let mut still_deferred = Vec::new();
                for completion in deferred_completions.drain(..) {
                    if current_tool_call_ids.contains(&completion.tool_call_id) {
                        pending_tool_requests.remove(&completion.tool_call_id);
                        restored.push(ClaudeHistoryReplayItem::ToolExecutionCompleted(completion));
                    } else {
                        still_deferred.push(completion);
                    }
                }
                deferred_completions = still_deferred;
            }
        }

        for completion in extract_tool_result_events_from_message(
            &message_value,
            &tool_name_by_id,
            &tool_call_by_id,
        ) {
            if !tool_call_by_id.contains_key(&completion.tool_call_id) {
                continue;
            }
            if auto_closed_tool_requests.contains(&completion.tool_call_id) {
                continue;
            }
            if emitted_tool_requests.contains(&completion.tool_call_id) {
                pending_tool_requests.remove(&completion.tool_call_id);
                restored.push(ClaudeHistoryReplayItem::ToolExecutionCompleted(completion));
            } else {
                deferred_completions.push(completion);
            }
        }
    }

    flush_unresolved_replay_tool_requests(
        &mut restored,
        &mut pending_tool_requests,
        &mut auto_closed_tool_requests,
    );

    if !deferred_completions.is_empty() {
        tracing::debug!(
            count = deferred_completions.len(),
            "skipping Claude replay tool completions whose requests were never replayed"
        );
    }

    commit_replay_invocation_usage(
        &mut cumulative_usage,
        &mut cumulative_usage_complete,
        &mut invocation_usage,
        &mut invocation_usage_complete,
        &mut invocation_message_ids,
    );

    ClaudeSessionReplay {
        items: restored,
        cumulative_usage,
        cumulative_usage_complete,
        conversation_bytes_total,
    }
}

fn replay_top_level_user_prompt_id(value: &Value) -> Option<String> {
    if value.get("type").and_then(Value::as_str) != Some("user")
        || value.get("isSidechain").and_then(Value::as_bool) != Some(false)
        || value
            .get("uuid")
            .and_then(Value::as_str)
            .and_then(normalize_nonempty)
            .is_none()
    {
        return None;
    }
    value
        .get("promptId")
        .and_then(Value::as_str)
        .and_then(normalize_nonempty)
}

fn commit_replay_invocation_usage(
    cumulative_usage: &mut Option<Value>,
    cumulative_usage_complete: &mut bool,
    invocation_usage: &mut Option<Value>,
    invocation_usage_complete: &mut bool,
    invocation_message_ids: &mut HashSet<String>,
) {
    if *invocation_usage_complete {
        if let Some(usage) = invocation_usage.take() {
            *cumulative_usage = Some(add_token_usage(cumulative_usage.as_ref(), &usage));
        }
    } else {
        *cumulative_usage_complete = false;
        *invocation_usage = None;
    }
    *invocation_usage_complete = true;
    invocation_message_ids.clear();
}

fn flush_unresolved_replay_tool_requests(
    restored: &mut Vec<ClaudeHistoryReplayItem>,
    pending_tool_requests: &mut HashMap<String, ClaudeToolCall>,
    auto_closed_tool_requests: &mut HashSet<String>,
) {
    if pending_tool_requests.is_empty() {
        return;
    }

    let mut pending = pending_tool_requests
        .drain()
        .map(|(_, tool_call)| tool_call)
        .collect::<Vec<_>>();
    pending.sort_by(|left, right| left.id.cmp(&right.id));

    for tool_call in pending {
        auto_closed_tool_requests.insert(tool_call.id.clone());
        restored.push(ClaudeHistoryReplayItem::ToolExecutionCompleted(
            ClaudeReplayToolExecution {
                tool_call_id: tool_call.id,
                tool_name: tool_call.name,
                success: false,
                tool_result: json!({
                    "kind": "Error",
                    "short_message": "Tool execution was interrupted",
                    "detailed_message": "Claude history did not contain a tool_result before the conversation advanced; treating the tool as interrupted.",
                }),
                error: Some(
                    "Claude history did not contain a tool_result before the conversation advanced; treating the tool as interrupted."
                        .to_string(),
                ),
            },
        ));
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
                    match exit_plan_mode_plan_info_from_tool_calls(tool_call_by_id.values()) {
                        Some(info) => json!({
                            "kind": "Other",
                            "result": {
                                "plan_content": info.plan,
                                "plan_path": info.plan_path,
                            }
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
    // Use the CLI's family aliases rather than pinned model IDs so we always
    // resolve to whatever the installed CLI considers the latest opus/sonnet/
    // haiku. The concrete model is reported back in the stream-start event, and
    // the context-window lookup keys off the family hint, so no pinned IDs are
    // needed here.
    let models = [
        ("opus", "Opus (latest)", true),
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
    // Fable ships a 1M context window by default (no explicit [1m] suffix).
    if normalized.contains("fable") {
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
    if let Some(root) = workspace_roots
        .iter()
        .find(|root| !root.trim().is_empty() && !root.trim_start().starts_with("ssh://"))
        .cloned()
    {
        return Ok(root);
    }
    if workspace_roots
        .iter()
        .any(|root| !root.trim().is_empty() && root.trim_start().starts_with("ssh://"))
    {
        return Err("Claude backend requires at least one local workspace root".to_string());
    }
    crate::backend::tyde_owned_no_root_cwd("claude")
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
) -> Result<ClaudeSessionReplay, ClaudeSessionHistoryError> {
    use crate::remote::{run_ssh_raw, shell_quote_arg};

    let encoded = encode_workspace_root(workspace_root);
    let id = normalize_nonempty(session_id)
        .ok_or_else(|| ClaudeSessionHistoryError::other("Invalid session id".to_string()))?;
    let relative_path = format!(".claude/projects/{encoded}/{id}.jsonl");
    let quoted_relative_path = shell_quote_arg(&relative_path);
    let cmd = format!(
        "file=\"$HOME\"/{quoted_relative_path}; \
         [ -f \"$file\" ] || {{ echo \"Claude session file is missing: $file\" >&2; exit 66; }}; \
         cat \"$file\""
    );
    let output = run_ssh_raw(host, &cmd).await.map_err(|err| {
        ClaudeSessionHistoryError::other(format!(
            "Failed to read remote Claude session '{id}': {err}"
        ))
    })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if output.status.code() == Some(66) || is_remote_missing_session_stderr(&stderr) {
            return Err(ClaudeSessionHistoryError::missing(
                format!("{host}:~/{relative_path}"),
                stderr.trim().to_string(),
            ));
        }
        return Err(ClaudeSessionHistoryError::other(format!(
            "Failed to read remote Claude session '{id}': {stderr}"
        )));
    }
    let contents = String::from_utf8_lossy(&output.stdout);
    Ok(parse_claude_session_replay(&contents))
}

fn is_remote_missing_session_stderr(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    lower.contains("no such file or directory")
        || lower.contains("does not exist")
        || (lower.contains("cannot open") && lower.contains("no such file"))
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

fn notify_turn_quiesced(waiters: Vec<oneshot::Sender<()>>) {
    for waiter in waiters {
        let _ = waiter.send(());
    }
}

// ---------------------------------------------------------------------------
// Backend trait implementation
// ---------------------------------------------------------------------------

use protocol::{
    AgentInput, BackendKind, ChatEvent, ChatMessage, MessageSender, SelectOption,
    SessionSettingField, SessionSettingFieldType, SessionSettingValue, SessionSettingsSchema,
    SpawnCostHint,
};

use super::{
    Backend, BackendSession, BackendSpawnConfig, BackendStartupError, EventStream,
    protocol_images_to_attachments, resolve_settings as resolve_backend_settings,
    session_settings_to_json,
};

type ClaudeReadyTx = Arc<Mutex<Option<oneshot::Sender<Result<(), String>>>>>;

fn claude_permission_mode_for_access_mode(access_mode: BackendAccessMode) -> &'static str {
    match access_mode {
        BackendAccessMode::Unrestricted => CLAUDE_DEFAULT_PERMISSION_MODE,
        BackendAccessMode::ReadOnly => CLAUDE_READ_ONLY_PERMISSION_MODE,
    }
}

/// Minimal Backend-trait handle for the Claude CLI.
///
/// Holds an `mpsc::UnboundedSender<AgentInput>` that the spawned task reads from;
/// the task writes stdin of the child process accordingly.
pub struct ClaudeBackend {
    input_tx: mpsc::UnboundedSender<AgentInput>,
    interrupt_tx: mpsc::UnboundedSender<ClaudeInterrupt>,
    session_id: Arc<std::sync::Mutex<Option<SessionId>>>,
    subagent_emitter_tx: watch::Sender<Option<Arc<dyn SubAgentEmitter>>>,
}

struct ClaudeInterrupt {
    reply: oneshot::Sender<bool>,
}

impl ClaudeBackend {
    pub(crate) async fn set_subagent_emitter(&self, emitter: Arc<dyn SubAgentEmitter>) {
        let _ = self.subagent_emitter_tx.send(Some(emitter));
    }

    pub(crate) async fn spawn_with_subagent_emitter(
        workspace_roots: Vec<String>,
        config: BackendSpawnConfig,
        initial_input: protocol::SendMessagePayload,
        emitter: Arc<dyn SubAgentEmitter>,
    ) -> Result<(Self, EventStream), String> {
        Self::spawn_with_initial_emitter(workspace_roots, config, initial_input, Some(emitter))
            .await
    }

    async fn spawn_with_initial_emitter(
        workspace_roots: Vec<String>,
        config: BackendSpawnConfig,
        initial_input: protocol::SendMessagePayload,
        initial_emitter: Option<Arc<dyn SubAgentEmitter>>,
    ) -> Result<(Self, EventStream), String> {
        Self::spawn_or_fork_with_initial_emitter(
            workspace_roots,
            config,
            None,
            initial_input,
            initial_emitter,
        )
        .await
    }

    async fn fork_with_initial_emitter(
        workspace_roots: Vec<String>,
        config: BackendSpawnConfig,
        from_session_id: SessionId,
        initial_input: protocol::SendMessagePayload,
        initial_emitter: Option<Arc<dyn SubAgentEmitter>>,
    ) -> Result<(Self, EventStream), String> {
        Self::spawn_or_fork_with_initial_emitter(
            workspace_roots,
            config,
            Some(from_session_id),
            initial_input,
            initial_emitter,
        )
        .await
    }

    async fn spawn_or_fork_with_initial_emitter(
        workspace_roots: Vec<String>,
        config: BackendSpawnConfig,
        fork_from_session_id: Option<SessionId>,
        initial_input: protocol::SendMessagePayload,
        initial_emitter: Option<Arc<dyn SubAgentEmitter>>,
    ) -> Result<(Self, EventStream), String> {
        let (input_tx, mut input_rx) = mpsc::unbounded_channel::<AgentInput>();
        let (interrupt_tx, mut interrupt_rx) = mpsc::unbounded_channel::<ClaudeInterrupt>();
        let (events_tx, events_rx) = mpsc::unbounded_channel::<ChatEvent>();
        let session_id = Arc::new(std::sync::Mutex::new(None));
        let session_id_task = Arc::clone(&session_id);
        let (subagent_emitter_tx, mut subagent_emitter_rx) = watch::channel(initial_emitter);
        let (ready_tx, ready_rx) = oneshot::channel::<Result<(), String>>();
        let (startup_cancel_tx, mut startup_cancel_rx) = oneshot::channel();
        let mut startup_cancel_guard = ClaudeDetachedStartupCancelGuard(Some(startup_cancel_tx));

        tokio::spawn(async move {
            let steering_content = claude_steering_content(&config);
            let agent_identity = claude_agent_identity(&config);
            let session_result = if let Some(from_session_id) = fork_from_session_id.as_ref() {
                ClaudeSession::fork(
                    &workspace_roots,
                    ClaudeForkConfig {
                        from_session_id: &from_session_id.0,
                        ssh_host: None,
                        startup_mcp_servers: &config.startup_mcp_servers,
                        steering_content: steering_content.as_deref(),
                        agent_identity: agent_identity.as_ref(),
                        tool_policy: config.resolved_spawn_config.tool_policy.clone(),
                        access_mode: config.resolved_spawn_config.access_mode,
                    },
                )
                .await
            } else {
                ClaudeSession::spawn(
                    &workspace_roots,
                    None,
                    &config.startup_mcp_servers,
                    steering_content.as_deref(),
                    agent_identity.as_ref(),
                    config.resolved_spawn_config.tool_policy.clone(),
                    config.resolved_spawn_config.access_mode,
                )
                .await
            };
            let (session, mut raw_events) = match session_result {
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
                    "permission_mode": claude_permission_mode_for_access_mode(
                        config.resolved_spawn_config.access_mode,
                    ),
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

            let initial_prompt = handle.send_message_payload(initial_input);
            tokio::pin!(initial_prompt);
            tokio::select! {
                biased;
                _ = &mut startup_cancel_rx => {
                    session.shutdown().await;
                    let _ = forward_task.await;
                    return;
                }
                result = &mut initial_prompt => {
                    if let Err(err) = result {
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
                }
            }

            loop {
                tokio::select! {
                    biased;
                    interrupt = interrupt_rx.recv() => {
                        let Some(interrupt) = interrupt else {
                            break;
                        };
                        let interrupted = match handle.execute(SessionCommand::CancelConversation).await {
                            Ok(()) => true,
                            Err(err) => {
                                tracing::error!("Failed to interrupt Claude turn: {err}");
                                false
                            }
                        };
                        let _ = interrupt.reply.send(interrupted);
                        if !interrupted {
                            break;
                        }
                    }
                    incoming = input_rx.recv() => {
                        let Some(input) = incoming else {
                            break;
                        };
                        match input {
                            AgentInput::SendMessage(payload) => {
                                if let Err(err) = handle.send_message_payload(payload).await {
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
        startup_cancel_guard.disarm();

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
}

fn claude_backend_defaults(
    cost_hint: Option<SpawnCostHint>,
) -> (Option<&'static str>, Option<ClaudeEffort>) {
    match cost_hint {
        Some(SpawnCostHint::Low) => (Some("haiku"), Some(ClaudeEffort::Low)),
        // Medium is a legacy no-op: spawn on the backend's own defaults.
        Some(SpawnCostHint::Medium) => (None, None),
        Some(SpawnCostHint::High) => (Some("opus"), Some(ClaudeEffort::Max)),
        None => (None, None),
    }
}

pub(crate) fn claude_cost_hint_defaults(
    cost_hint: SpawnCostHint,
) -> protocol::SessionSettingsValues {
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
            SessionSettingValue::String(effort.as_str().to_string()),
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
        message_id: None,
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
    if config.resolved_spawn_config.access_mode == BackendAccessMode::ReadOnly {
        sections.push(READ_ONLY_ACCESS_MODE_INSTRUCTIONS.to_string());
    }
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
    events_tx: &mpsc::UnboundedSender<ChatEvent>,
    session_id_sink: &Arc<std::sync::Mutex<Option<SessionId>>>,
    ready_tx: Option<&ClaudeReadyTx>,
) -> bool {
    if let Ok(event) = serde_json::from_value::<ChatEvent>(raw.clone()) {
        return events_tx.send(event).is_ok();
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
                .is_err()
            {
                return false;
            }
        }
        _ => {}
    }

    true
}

fn claude_capacity_access_from_initialize(response: &Value) -> ClaudeCapacityAccess {
    let Some(account) = response.get("account").filter(|value| value.is_object()) else {
        return ClaudeCapacityAccess::Unknown;
    };
    match account.get("apiProvider").and_then(Value::as_str) {
        Some("firstParty") => {
            if account
                .get("subscriptionType")
                .and_then(Value::as_str)
                .and_then(normalize_nonempty)
                .is_some()
            {
                ClaudeCapacityAccess::Subscription
            } else {
                ClaudeCapacityAccess::ApiKey
            }
        }
        Some(_) => ClaudeCapacityAccess::ExternalProvider,
        None => ClaudeCapacityAccess::Unknown,
    }
}

fn parse_claude_usage_reset(
    value: Option<&Value>,
) -> Result<CapacityReset, CapacityUnavailableReason> {
    let Some(value) = value else {
        return Ok(CapacityReset::NotReported);
    };
    if value.is_null() {
        return Ok(CapacityReset::NotReported);
    }
    let timestamp = value
        .as_str()
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .and_then(|value| u64::try_from(value.timestamp_millis()).ok())
        .ok_or(CapacityUnavailableReason::MalformedReport)?;
    Ok(CapacityReset::At { at_ms: timestamp })
}

pub(crate) fn map_claude_control_usage(
    response: &Value,
) -> Result<CapacityReport, CapacityUnavailableReason> {
    if response
        .get("rate_limits_available")
        .and_then(Value::as_bool)
        != Some(true)
    {
        return Err(CapacityUnavailableReason::MalformedReport);
    }
    let rate_limits = response
        .get("rate_limits")
        .filter(|value| value.is_object())
        .ok_or(CapacityUnavailableReason::MalformedReport)?;
    let limits = rate_limits
        .get("limits")
        .and_then(Value::as_array)
        .ok_or(CapacityUnavailableReason::MalformedReport)?;
    let mut buckets = Vec::with_capacity(limits.len());
    for limit in limits {
        let kind = limit
            .get("kind")
            .and_then(Value::as_str)
            .ok_or(CapacityUnavailableReason::MalformedReport)?;
        let percent = limit
            .get("percent")
            .and_then(Value::as_f64)
            .filter(|value| (0.0..=100.0).contains(value))
            .ok_or(CapacityUnavailableReason::MalformedReport)?;
        let used_percent = percent.round() as u8;
        let (id, label, window) = match kind {
            "session" => (
                CapacityBucketId::Claude {
                    limit: ClaudeLimitType::FiveHour,
                },
                "session limit".to_string(),
                CapacityWindow::Rolling {
                    duration_minutes: 5 * 60,
                },
            ),
            "weekly_all" => (
                CapacityBucketId::Claude {
                    limit: ClaudeLimitType::SevenDay,
                },
                "weekly limit".to_string(),
                CapacityWindow::Rolling {
                    duration_minutes: 7 * 24 * 60,
                },
            ),
            "weekly_scoped" => {
                let model = limit
                    .pointer("/scope/model/display_name")
                    .and_then(Value::as_str)
                    .and_then(normalize_nonempty)
                    .ok_or(CapacityUnavailableReason::MalformedReport)?;
                (
                    CapacityBucketId::ClaudeModel {
                        name: model.clone(),
                    },
                    format!("{model} limit"),
                    CapacityWindow::Rolling {
                        duration_minutes: 7 * 24 * 60,
                    },
                )
            }
            "overage" | "extra_usage" => (
                CapacityBucketId::Claude {
                    limit: ClaudeLimitType::Overage,
                },
                "usage credits".to_string(),
                CapacityWindow::NotReported,
            ),
            _ => return Err(CapacityUnavailableReason::MalformedReport),
        };
        buckets.push(CapacityBucket {
            id,
            label,
            measure: CapacityMeasure::UsedPercent {
                used_percent,
                remaining_percent: 100 - used_percent,
                provenance: ValueProvenance {
                    vendor_reported: true,
                },
            },
            scope: CapacityScope::Account,
            window,
            reset: parse_claude_usage_reset(limit.get("resets_at"))?,
            status: None,
        });
    }
    if buckets.is_empty() {
        return Err(CapacityUnavailableReason::MalformedReport);
    }
    let plan = response
        .get("subscription_type")
        .and_then(Value::as_str)
        .and_then(normalize_nonempty)
        .map(|label| protocol::CapacityPlanLabel { label });
    Ok(CapacityReport {
        source: CapacitySource::ClaudeControlUsage,
        observed_at_ms: None,
        plan,
        buckets,
        coverage: CapacityCoverage::AllVendorBuckets,
    })
}

/// Maps Claude's already-received stream-json frame. The frame contains one
/// vendor-selected binding bucket, never an inferred account-wide aggregate.
pub(crate) fn map_passive_rate_limit_event(
    frame: &Value,
) -> Result<CapacityReport, CapacityUnavailableReason> {
    let info = frame
        .get("rate_limit_info")
        .filter(|value| value.is_object())
        .ok_or(CapacityUnavailableReason::MalformedReport)?;
    let base_status = match info.get("status").and_then(Value::as_str) {
        Some("allowed") => CapacityBucketStatus::Allowed,
        Some("allowed_warning") => CapacityBucketStatus::AllowedWarning,
        Some("rejected") => CapacityBucketStatus::Rejected,
        _ => return Err(CapacityUnavailableReason::MalformedReport),
    };
    let limit = match info.get("rateLimitType").and_then(Value::as_str) {
        Some("five_hour") => ClaudeLimitType::FiveHour,
        Some("seven_day") => ClaudeLimitType::SevenDay,
        Some("seven_day_opus") => ClaudeLimitType::SevenDayOpus,
        Some("seven_day_sonnet") => ClaudeLimitType::SevenDaySonnet,
        Some("seven_day_overage_included") => ClaudeLimitType::SevenDayOverageIncluded,
        Some("overage") => ClaudeLimitType::Overage,
        _ => return Err(CapacityUnavailableReason::MalformedReport),
    };
    let status = if matches!(limit, ClaudeLimitType::Overage) {
        match info.get("overageStatus").and_then(Value::as_str) {
            None => base_status,
            Some("allowed") => CapacityBucketStatus::Allowed,
            Some("allowed_warning") => CapacityBucketStatus::AllowedWarning,
            Some("rejected") => CapacityBucketStatus::Rejected,
            Some(_) => return Err(CapacityUnavailableReason::MalformedReport),
        }
    } else {
        base_status
    };
    let label = match limit {
        ClaudeLimitType::FiveHour => "session limit",
        ClaudeLimitType::SevenDay => "weekly limit",
        ClaudeLimitType::SevenDayOverageIncluded => "Fable 5 limit",
        ClaudeLimitType::SevenDayOpus => "Opus limit",
        ClaudeLimitType::SevenDaySonnet => "Sonnet limit",
        ClaudeLimitType::Overage => "overage limit",
    };
    let measure = match info.get("utilization") {
        None | Some(Value::Null) => CapacityMeasure::ReportedWithoutMagnitude,
        Some(value) => {
            let utilization = value
                .as_f64()
                .filter(|value| (0.0..=1.0).contains(value))
                .ok_or(CapacityUnavailableReason::MalformedReport)?;
            let used_percent = (utilization * 100.0).round() as u8;
            CapacityMeasure::UsedPercent {
                used_percent,
                remaining_percent: 100 - used_percent,
                provenance: ValueProvenance {
                    vendor_reported: true,
                },
            }
        }
    };
    let reset_key = if matches!(limit, ClaudeLimitType::Overage) {
        "overageResetsAt"
    } else {
        "resetsAt"
    };
    let reset = match info.get(reset_key) {
        None | Some(Value::Null) => CapacityReset::NotReported,
        Some(value) => value
            .as_u64()
            .and_then(|seconds| seconds.checked_mul(1000))
            .map(|at_ms| CapacityReset::At { at_ms })
            .ok_or(CapacityUnavailableReason::MalformedReport)?,
    };
    Ok(CapacityReport {
        source: CapacitySource::ClaudeRateLimitEvent,
        observed_at_ms: None,
        plan: None,
        buckets: vec![CapacityBucket {
            id: CapacityBucketId::Claude { limit },
            label: label.to_string(),
            measure,
            scope: CapacityScope::NotReported,
            window: CapacityWindow::NotReported,
            reset,
            status: Some(status),
        }],
        coverage: CapacityCoverage::RepresentativeBucketOnly,
    })
}

/// Route only Claude's existing stream-json capacity event through the
/// session-owned emitter. It intentionally performs no read, refresh, or
/// credential access.
pub(crate) fn forward_passive_rate_limit_event(
    frame: &Value,
    emitter: &dyn SubAgentEmitter,
) -> bool {
    if frame.get("type").and_then(Value::as_str) != Some("rate_limit_event") {
        return false;
    }
    let state = match map_passive_rate_limit_event(frame) {
        Ok(report) => protocol::BackendCapacityState::Known { report },
        Err(reason) => protocol::BackendCapacityState::Unavailable { reason },
    };
    emitter.on_backend_capacity(protocol::BackendKind::Claude, state);
    true
}

#[cfg(test)]
mod capacity_mapping_tests {
    use super::*;

    #[test]
    fn initialize_account_distinguishes_subscription_api_key_and_external_provider() {
        assert_eq!(
            claude_capacity_access_from_initialize(&json!({"account": {
                "apiProvider": "firstParty", "subscriptionType": "Claude Max"
            }})),
            ClaudeCapacityAccess::Subscription
        );
        assert_eq!(
            claude_capacity_access_from_initialize(&json!({"account": {
                "apiProvider": "firstParty", "subscriptionType": null
            }})),
            ClaudeCapacityAccess::ApiKey
        );
        assert_eq!(
            claude_capacity_access_from_initialize(&json!({"account": {
                "apiProvider": "bedrock", "subscriptionType": null
            }})),
            ClaudeCapacityAccess::ExternalProvider
        );
    }

    #[test]
    fn control_usage_maps_all_cli_reported_limits() {
        let report = map_claude_control_usage(&json!({
            "subscription_type": "max",
            "rate_limits_available": true,
            "rate_limits": {"limits": [
                {"kind": "session", "percent": 2,
                 "resets_at": "2026-07-17T21:20:00.325356+00:00"},
                {"kind": "weekly_all", "percent": 15,
                 "resets_at": "2026-07-22T18:00:00.325379+00:00"},
                {"kind": "weekly_scoped", "percent": 8,
                 "resets_at": "2026-07-22T18:00:00.325737+00:00",
                 "scope": {"model": {"display_name": "Fable"}}}
            ]}
        }))
        .expect("authoritative Claude usage");
        assert_eq!(report.source, CapacitySource::ClaudeControlUsage);
        assert_eq!(report.coverage, CapacityCoverage::AllVendorBuckets);
        assert_eq!(report.buckets.len(), 3);
        assert_eq!(report.buckets[0].label, "session limit");
        assert_eq!(report.buckets[1].label, "weekly limit");
        assert_eq!(report.buckets[2].label, "Fable limit");
        assert!(matches!(
            report.buckets[2].id,
            CapacityBucketId::ClaudeModel { ref name } if name == "Fable"
        ));
        assert!(matches!(
            report.buckets[2].measure,
            CapacityMeasure::UsedPercent {
                used_percent: 8,
                remaining_percent: 92,
                ..
            }
        ));
    }

    #[test]
    fn control_usage_rejects_partial_or_unavailable_reports() {
        assert_eq!(
            map_claude_control_usage(&json!({"rate_limits_available": false})),
            Err(CapacityUnavailableReason::MalformedReport)
        );
        assert_eq!(
            map_claude_control_usage(&json!({
                "rate_limits_available": true,
                "rate_limits": {"limits": [{"kind": "session", "percent": 101}]}
            })),
            Err(CapacityUnavailableReason::MalformedReport)
        );
    }

    #[test]
    fn representative_rate_limit_event_converts_fraction_and_drops_internal_overage_fields() {
        let report = map_passive_rate_limit_event(&json!({
            "type": "rate_limit_event",
            "rate_limit_info": {
                "status": "allowed_warning", "rateLimitType": "seven_day_opus",
                "utilization": 0.82, "resetsAt": 1_700_000_000,
                "overageStatus": "rejected", "overagePeriodMonthly": {"utilization": 0.99},
                "overagePeriodChannel": {"utilization": 0.98}
            }
        }))
        .expect("representative event");
        assert_eq!(
            report.coverage,
            protocol::CapacityCoverage::RepresentativeBucketOnly
        );
        assert_eq!(report.buckets.len(), 1);
        assert_eq!(
            report.buckets[0].status,
            Some(protocol::CapacityBucketStatus::AllowedWarning)
        );
        assert!(matches!(
            &report.buckets[0].measure,
            protocol::CapacityMeasure::UsedPercent {
                used_percent: 82,
                remaining_percent: 18,
                provenance: protocol::ValueProvenance {
                    vendor_reported: true
                },
            }
        ));
        assert_eq!(
            report.buckets[0].measure.used_percent_provenance(),
            Some(protocol::PercentValueProvenance::VendorReported)
        );
        assert_eq!(
            report.buckets[0].measure.remaining_percent_provenance(),
            Some(protocol::PercentValueProvenance::DerivedComplement)
        );
        assert!(matches!(
            &report.buckets[0].reset,
            protocol::CapacityReset::At {
                at_ms: 1_700_000_000_000
            }
        ));
    }

    #[test]
    fn malformed_or_out_of_range_rate_limit_event_is_typed_and_sanitized() {
        assert_eq!(
            map_passive_rate_limit_event(&json!({"rate_limit_info": {
                "status":"allowed", "rateLimitType":"five_hour", "utilization":1.01
            }})),
            Err(protocol::CapacityUnavailableReason::MalformedReport)
        );
        assert_eq!(
            map_passive_rate_limit_event(&json!({"rate_limit_info": {
                "status":"allowed", "rateLimitType":"five_hour", "resetsAt":"tomorrow"
            }})),
            Err(protocol::CapacityUnavailableReason::MalformedReport)
        );
        assert_eq!(
            map_passive_rate_limit_event(&json!({"rate_limit_info": {
                "status":"allowed", "rateLimitType":"five_hour", "resetsAt":18446744073709551615_u64
            }})),
            Err(protocol::CapacityUnavailableReason::MalformedReport)
        );
    }

    #[test]
    fn overage_bucket_uses_vendor_overage_status_and_reset() {
        let report = map_passive_rate_limit_event(&json!({"rate_limit_info": {
            "status":"allowed", "rateLimitType":"overage", "utilization":0.5,
            "overageStatus":"rejected", "overageResetsAt":42
        }}))
        .expect("overage event");
        assert_eq!(
            report.buckets[0].status,
            Some(protocol::CapacityBucketStatus::Rejected)
        );
        assert!(matches!(
            &report.buckets[0].reset,
            protocol::CapacityReset::At { at_ms: 42_000 }
        ));
    }

    #[test]
    fn overage_included_bucket_keeps_its_distinct_vendor_label() {
        let report = map_passive_rate_limit_event(&json!({"rate_limit_info": {
            "status":"allowed", "rateLimitType":"seven_day_overage_included"
        }}))
        .expect("overage-included event");
        assert_eq!(report.buckets[0].label, "Fable 5 limit");
    }
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
                    select_options_by_setting: None,
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
                            SelectOption {
                                value: "fable".to_string(),
                                label: "Fable".to_string(),
                            },
                        ],
                        default: None,
                        nullable: true,
                    },
                },
                SessionSettingField {
                    key: "effort".to_string(),
                    label: "Effort".to_string(),
                    description: None,
                    use_slider: true,
                    select_options_by_setting: None,
                    field_type: SessionSettingFieldType::Select {
                        options: ClaudeEffort::ALL
                            .iter()
                            .map(|effort| SelectOption {
                                value: effort.as_str().to_string(),
                                label: effort.label().to_string(),
                            })
                            .collect(),
                        default: None,
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
        Self::spawn_with_initial_emitter(workspace_roots, config, initial_input, None).await
    }

    async fn resume(
        workspace_roots: Vec<String>,
        config: BackendSpawnConfig,
        session_id: protocol::SessionId,
    ) -> Result<(Self, EventStream), String> {
        let (input_tx, mut input_rx) = mpsc::unbounded_channel::<AgentInput>();
        let (interrupt_tx, mut interrupt_rx) = mpsc::unbounded_channel::<ClaudeInterrupt>();
        let (events_tx, events_rx) = mpsc::unbounded_channel::<ChatEvent>();
        let (resume_replay_complete_tx, resume_replay_complete_rx) =
            tokio::sync::oneshot::channel();
        let (subagent_emitter_tx, mut subagent_emitter_rx) =
            watch::channel::<Option<Arc<dyn SubAgentEmitter>>>(None);

        let session_id = session_id.0;
        let backend_session_id =
            Arc::new(std::sync::Mutex::new(Some(SessionId(session_id.clone()))));
        let backend_session_id_task = Arc::clone(&backend_session_id);

        let steering_content = claude_steering_content(&config);
        let agent_identity = claude_agent_identity(&config);
        let (session, mut raw_events) = ClaudeSession::spawn(
            &workspace_roots,
            None,
            &config.startup_mcp_servers,
            steering_content.as_deref(),
            agent_identity.as_ref(),
            config.resolved_spawn_config.tool_policy.clone(),
            config.resolved_spawn_config.access_mode,
        )
        .await
        .map_err(|err| format!("Failed to spawn Claude resume session: {err}"))?;
        let mut startup_guard = ClaudeResumeStartupGuard::new(session.clone());

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
                "permission_mode": claude_permission_mode_for_access_mode(
                    config.resolved_spawn_config.access_mode,
                ),
            });
            if let Err(err) = handle
                .execute(SessionCommand::UpdateSettings {
                    settings,
                    persist: false,
                })
                .await
            {
                startup_guard.disarm();
                session.shutdown().await;
                return Err(format!("Failed to configure resumed Claude session: {err}"));
            }
        }

        if let Err(err) = handle
            .execute(SessionCommand::ResumeSession { session_id })
            .await
        {
            startup_guard.disarm();
            session.shutdown().await;
            return Err(format!("Failed to resume Claude session: {err}"));
        }
        // The agent starts its replay-barrier timeout only after `resume`
        // returns, so the CLI's independent initialization window must finish
        // before the EventStream and its ready barrier become observable.
        if let Err(err) = session.inner.ensure_process_ready().await {
            startup_guard.disarm();
            session.shutdown().await;
            return Err(format!(
                "Failed to initialize resumed Claude session: {err}"
            ));
        }

        while let Ok(raw) = raw_events.try_recv() {
            if !forward_claude_backend_event(raw, &events_tx, &backend_session_id_task, None).await
            {
                startup_guard.disarm();
                session.shutdown().await;
                return Err("Claude resume event stream closed during replay".to_string());
            }
        }
        let _ = resume_replay_complete_tx.send(());
        startup_guard.disarm();

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    interrupt = interrupt_rx.recv() => {
                        let Some(interrupt) = interrupt else {
                            break;
                        };
                        let interrupted = match handle.execute(SessionCommand::CancelConversation).await {
                            Ok(()) => true,
                            Err(err) => {
                                tracing::error!("Failed to interrupt resumed Claude turn: {err}");
                                false
                            }
                        };
                        let _ = interrupt.reply.send(interrupted);
                        if !interrupted {
                            break;
                        }
                    }
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
                                if let Err(err) = handle.send_message_payload(payload).await {
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
            EventStream::new_with_resume_replay_barrier(events_rx, resume_replay_complete_rx),
        ))
    }

    async fn fork(
        workspace_roots: Vec<String>,
        config: BackendSpawnConfig,
        from_session_id: protocol::SessionId,
        initial_input: protocol::SendMessagePayload,
    ) -> Result<(Self, EventStream), BackendStartupError> {
        Self::fork_with_initial_emitter(
            workspace_roots,
            config,
            from_session_id,
            initial_input,
            None,
        )
        .await
        .map_err(BackendStartupError::backend_failed)
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
        self.input_tx.send(input).is_ok()
    }

    async fn interrupt(&self) -> bool {
        let (reply, done) = oneshot::channel();
        if self.interrupt_tx.send(ClaudeInterrupt { reply }).is_err() {
            return false;
        }
        // Claude intentionally provides stronger semantics than the Backend
        // trait baseline: for the deferred-cancel race,
        // ClaudeBackend::interrupt().await is a quiescence barrier.
        done.await.unwrap_or(false)
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
    use crate::backend::Backend;
    use protocol::{ChatEvent, ToolExecutionNormalizationFailure, ToolRequestType};
    use tokio::sync::oneshot;
    use tokio::time::{Duration, timeout};

    const FAKE_CLAUDE_EXIT_TIMEOUT: Duration = Duration::from_secs(10);

    fn make_image(data: &str, media_type: &str) -> ImageAttachment {
        ImageAttachment {
            data: data.to_string(),
            media_type: media_type.to_string(),
            name: "image".to_string(),
            size: data.len() as u64,
        }
    }

    fn make_test_inner() -> (ClaudeInner, mpsc::UnboundedReceiver<Value>) {
        make_test_inner_with_workspace("/tmp/test-workspace".to_string())
    }

    fn test_parent_emitter() -> (Arc<TurnEmitter>, mpsc::UnboundedReceiver<Value>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Arc::new(TurnEmitter::new_for_agent(tx, AgentName(CLAUDE_AGENT_NAME))),
            rx,
        )
    }

    fn make_test_inner_with_workspace(
        workspace_root: String,
    ) -> (ClaudeInner, mpsc::UnboundedReceiver<Value>) {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let inner = ClaudeInner {
            emitter: Arc::new(TurnEmitter::new_for_agent(
                event_tx,
                AgentName(CLAUDE_AGENT_NAME),
            )),
            state: Mutex::new(ClaudeState {
                workspace_root,
                ssh_host: None,
                session_id: None,
                fork_from_session_id: None,
                start_session_fresh: false,
                ephemeral: false,
                model: None,
                effort: None,
                permission_mode: None,
                startup_mcp_config_json: None,
                steering_content: None,
                agent_identity: None,
                tool_policy: ToolPolicy::Unrestricted,
                cumulative_usage: None,
                cumulative_usage_complete: true,
                conversation_bytes_total: 0,
                active_turn: None,
                restart_process_after_turn: false,
                subagent_emitter: None,
                capacity_access: ClaudeCapacityAccess::Unknown,
                capacity_refresh_in_flight: false,
                capacity_report_emitted: false,
                authoritative_capacity_emitted: false,
            }),
            runtime: Mutex::new(None),
            turn_event_gate: Mutex::new(()),
        };
        (inner, event_rx)
    }

    #[test]
    fn malformed_canonical_claude_request_marks_its_completion() {
        let (inner, mut rx) = make_test_inner();
        let tool_call = ClaudeToolCall {
            id: "claude-normalization".to_owned(),
            name: "mcp__tyde-agent-control__tyde_send_agent_message".to_owned(),
            arguments: json!({ "agent_id": "agent-a" }),
        };

        inner.emit_tool_request(&tool_call);
        inner.emit_tool_execution_completed(
            &tool_call.id,
            &tool_call.name,
            true,
            json!({ "kind": "Other", "result": { "ok": true } }),
            None,
        );

        let mut fallback_request = None;
        let mut completion = None;
        while let Ok(raw) = rx.try_recv() {
            if !matches!(
                raw.get("kind").and_then(Value::as_str),
                Some("ToolRequest" | "ToolExecutionCompleted")
            ) {
                continue;
            }
            let event: ChatEvent =
                serde_json::from_value(raw).expect("Claude emitter event is a ChatEvent");
            match event {
                ChatEvent::ToolRequest(request) => fallback_request = Some(request),
                ChatEvent::ToolExecutionCompleted(data) => completion = Some(data),
                _ => {}
            }
        }
        assert!(matches!(
            fallback_request.expect("fallback tool request").tool_type,
            ToolRequestType::Other { .. }
        ));
        assert_eq!(
            completion
                .expect("marked tool completion")
                .normalization_failure,
            Some(ToolExecutionNormalizationFailure::CanonicalRequest)
        );
    }

    #[test]
    fn claude_pick_workspace_root_uses_tyde_no_root_cwd_for_empty_roots() {
        let root = pick_workspace_root(&[]).expect("empty roots should resolve to no-root cwd");

        assert!(Path::new(&root).is_dir());
        assert!(Path::new(&root).ends_with(Path::new(".tyde").join("claude").join("no-root")));
    }

    #[test]
    fn claude_pick_workspace_root_keeps_ssh_only_roots_invalid() {
        let err = pick_workspace_root(&["ssh://devbox.example.com/workspace".to_string()])
            .expect_err("ssh-only local roots should remain invalid");

        assert!(err.contains("requires at least one local workspace root"));
    }

    #[tokio::test]
    async fn cancel_active_turn_waits_until_idle_is_published() {
        let (inner, mut rx) = make_test_inner();
        let inner = Arc::new(inner);
        let (outcome_tx, _outcome_rx) = oneshot::channel();

        {
            let mut state = inner.state.lock().await;
            state.active_turn = Some(ActiveTurn {
                id: 42,
                outcome_tx: Some(outcome_tx),
                interrupt_requested: false,
                pending_ask_user_question: None,
                pending_exit_plan_mode: None,
                quiesced_waiters: Vec::new(),
            });
        }

        let (done_tx, mut done_rx) = oneshot::channel();
        let cancel_inner = Arc::clone(&inner);
        tokio::spawn(async move {
            cancel_inner.cancel_active_turn().await;
            let _ = done_tx.send(());
        });

        timeout(Duration::from_secs(1), async {
            loop {
                if inner
                    .state
                    .lock()
                    .await
                    .active_turn
                    .as_ref()
                    .is_some_and(|active| active.interrupt_requested)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("interrupt flag should be set");
        assert!(
            timeout(Duration::from_millis(50), &mut done_rx)
                .await
                .is_err(),
            "interrupt must not resolve while active_turn is still set"
        );

        let waiters = inner.clear_active_turn(42).await;
        assert!(
            inner.state.lock().await.active_turn.is_none(),
            "active turn should be cleared before publishing idle"
        );
        assert!(
            timeout(Duration::from_millis(50), &mut done_rx)
                .await
                .is_err(),
            "interrupt must not resolve before OperationCancelled publishes idle"
        );

        inner.emit_operation_cancelled("Claude turn cancelled.");
        assert!(
            timeout(Duration::from_millis(50), &mut done_rx)
                .await
                .is_err(),
            "interrupt must not resolve before quiescence waiters are notified"
        );
        notify_turn_quiesced(waiters);

        timeout(Duration::from_secs(1), done_rx)
            .await
            .expect("interrupt should resolve after idle is published")
            .expect("interrupt completion channel should stay open");
        let cancelled = rx.recv().await.expect("cancelled event");
        assert_eq!(event_kind(&cancelled), Some("OperationCancelled"));
        let idle = rx.recv().await.expect("idle event");
        assert_eq!(event_kind(&idle), Some("TypingStatusChanged"));
        assert_eq!(idle.get("data").and_then(Value::as_bool), Some(false));
        assert!(
            rx.try_recv().is_err(),
            "cancelled turn should emit one idle"
        );
    }

    #[tokio::test]
    async fn operation_cancelled_after_mid_turn_stream_start_synthesizes_stream_end() {
        // Regression: when a cancel arrives after a mid-turn segment StreamStart
        // but before any content for that segment was emitted, the backend used
        // to emit OperationCancelled with no closing StreamEnd. That tripped
        // the protocol validator on the next turn's StreamStart. Per the
        // protocol spec (ChatEvent::OperationCancelled doc), cancel must first
        // close any open stream.
        let (inner, mut rx) = make_test_inner();
        inner.emit_stream_start("claude-msg-1-seg-1", None);
        let _ = rx.recv().await.expect("stream_start");
        inner.emit_operation_cancelled("Claude turn cancelled.");
        let first = rx.recv().await.expect("first event after cancel");
        assert_eq!(event_kind(&first), Some("StreamEnd"));
        let second = rx.recv().await.expect("second event after cancel");
        assert_eq!(event_kind(&second), Some("OperationCancelled"));
    }

    #[tokio::test]
    async fn terminal_cancel_emits_stream_end_then_tool_completed_then_cancelled() {
        // Full spec ordering: mid-turn, a segment StreamStart fires and a
        // tool_use arrives. Cancel races before the tool executes. The
        // terminal handler must emit events in this exact order:
        //   StreamEnd → ToolExecutionCompleted → OperationCancelled
        // (see ChatEvent docs in protocol/src/types.rs).
        let (inner, mut rx) = make_test_inner();
        let mut summary = ClaudeStdoutSummary::default();
        let mut segment = SegmentState::default();
        let base_id = "claude-msg-1".to_string();
        let mut current_id = base_id.clone();

        // Previous phase emitted a StreamEnd — simulate its effect.
        summary.emitted_phase_count = 1;
        // Mid-turn, Claude starts a new segment.
        inner.emit_stream_start("claude-msg-1-seg-1", None);
        let _ = rx.recv().await;

        // Inject an unresolved tool request for that segment.
        consume_claude_stream_value(
            &json!({
                "type": "stream_event",
                "event": {
                    "type": "content_block_start",
                    "index": 0,
                    "content_block": {
                        "type": "tool_use",
                        "id": "toolu_cancelled",
                        "name": "Bash",
                        "input": { "command": "sleep 9999" }
                    }
                }
            }),
            &mut summary,
            &mut segment,
            &inner,
            &base_id,
            &mut current_id,
        );
        // Drain anything consume_claude_stream_value emitted (nothing expected
        // here, but don't leak into the cancel sequence below).
        while rx.try_recv().is_ok() {}

        // Cancel fires the terminal path.
        inner
            .emit_terminal_phase_or_placeholder(&mut summary, 0, None, None, None)
            .await;
        inner.emit_operation_cancelled("Claude turn cancelled.");

        let first = rx.recv().await.expect("first");
        assert_eq!(event_kind(&first), Some("StreamEnd"));
        let second = rx.recv().await.expect("second");
        assert_eq!(event_kind(&second), Some("ToolRequest"));
        let third = rx.recv().await.expect("third");
        assert_eq!(event_kind(&third), Some("ToolExecutionCompleted"));
        let fourth = rx.recv().await.expect("fourth");
        assert_eq!(event_kind(&fourth), Some("OperationCancelled"));
    }

    #[tokio::test]
    async fn operation_cancelled_without_open_stream_does_not_synthesize_stream_end() {
        let (inner, mut rx) = make_test_inner();
        inner.emit_operation_cancelled("Claude turn cancelled.");
        // Protocol contract: cancel without an open stream emits the
        // OperationCancelled → TypingStatusChanged(false) tail only —
        // no synthesized StreamEnd.
        let first = rx.recv().await.expect("OperationCancelled");
        assert_eq!(event_kind(&first), Some("OperationCancelled"));
        let second = rx.recv().await.expect("TypingStatusChanged");
        assert_eq!(event_kind(&second), Some("TypingStatusChanged"));
        assert!(rx.try_recv().is_err(), "no synthesized StreamEnd expected");
    }

    #[test]
    fn claude_optional_session_settings_have_no_schema_defaults() {
        let schema = ClaudeBackend::session_settings_schema();

        let model_field = schema
            .fields
            .iter()
            .find(|field| field.key == "model")
            .expect("Claude schema should include model");
        let effort_field = schema
            .fields
            .iter()
            .find(|field| field.key == "effort")
            .expect("Claude schema should include effort");

        match &model_field.field_type {
            SessionSettingFieldType::Select {
                default, nullable, ..
            } => {
                assert!(*nullable, "Claude model should remain optional");
                assert_eq!(
                    default, &None,
                    "Claude model should stay unset until explicitly chosen"
                );
            }
            other => panic!("expected Claude model field to be Select, got {other:?}"),
        }

        match &effort_field.field_type {
            SessionSettingFieldType::Select {
                options,
                default,
                nullable,
            } => {
                assert!(*nullable, "Claude effort should remain optional");
                assert_eq!(
                    default, &None,
                    "Claude effort should stay unset until explicitly chosen"
                );
                assert_eq!(
                    options
                        .iter()
                        .map(|option| (option.value.as_str(), option.label.as_str()))
                        .collect::<Vec<_>>(),
                    vec![
                        ("low", "Low"),
                        ("medium", "Medium"),
                        ("high", "High"),
                        ("xhigh", "XHigh"),
                        ("max", "Max"),
                    ]
                );
            }
            other => panic!("expected Claude effort field to be Select, got {other:?}"),
        }
    }

    #[test]
    fn claude_resolve_session_settings_leaves_optional_fields_unset_by_default() {
        let resolved = resolve_session_settings(&BackendSpawnConfig::default());
        assert!(
            resolved.0.is_empty(),
            "Claude should not inject model or effort when the user left them unset"
        );
    }

    #[test]
    fn claude_cost_hint_still_sets_explicit_session_settings() {
        let resolved = resolve_session_settings(&BackendSpawnConfig {
            cost_hint: Some(SpawnCostHint::Low),
            ..BackendSpawnConfig::default()
        });

        assert_eq!(
            resolved.0.get("model"),
            Some(&SessionSettingValue::String("haiku".to_string()))
        );
        assert_eq!(
            resolved.0.get("effort"),
            Some(&SessionSettingValue::String("low".to_string()))
        );

        let resolved = resolve_session_settings(&BackendSpawnConfig {
            cost_hint: Some(SpawnCostHint::High),
            ..BackendSpawnConfig::default()
        });

        assert_eq!(
            resolved.0.get("model"),
            Some(&SessionSettingValue::String("opus".to_string()))
        );
        assert_eq!(
            resolved.0.get("effort"),
            Some(&SessionSettingValue::String("max".to_string()))
        );
    }

    #[test]
    fn claude_medium_cost_hint_is_a_no_op() {
        let resolved = resolve_session_settings(&BackendSpawnConfig {
            cost_hint: Some(SpawnCostHint::Medium),
            ..BackendSpawnConfig::default()
        });

        assert_eq!(resolved.0.get("model"), None);
        assert_eq!(resolved.0.get("effort"), None);
    }

    #[test]
    fn claude_read_only_access_mode_uses_accept_edits_permission_mode() {
        assert_eq!(
            claude_permission_mode_for_access_mode(BackendAccessMode::ReadOnly),
            "acceptEdits"
        );
        assert_eq!(
            claude_permission_mode_for_access_mode(BackendAccessMode::Unrestricted),
            "bypassPermissions"
        );
    }

    #[test]
    fn claude_read_only_steering_includes_shared_advisory() {
        let steering = claude_steering_content(&BackendSpawnConfig {
            resolved_spawn_config: crate::agent::customization::ResolvedSpawnConfig {
                access_mode: BackendAccessMode::ReadOnly,
                ..Default::default()
            },
            ..Default::default()
        })
        .expect("read-only advisory");

        assert!(steering.contains("Backend access mode is read-only (best effort)"));
        assert!(steering.contains("do not create, edit, or delete files"));
    }

    #[test]
    fn claude_cli_args_resume_valid_session_with_resume() {
        let args = build_claude_cli_args(&ClaudeProcessSpawnConfig {
            workspace_root: "/tmp/workspace".to_string(),
            ssh_host: None,
            session_id: Some("valid-session".to_string()),
            fork_from_session_id: None,
            resume_existing_session: true,
            ephemeral: false,
            model: None,
            effort: None,
            permission_mode: None,
            startup_mcp_config_json: None,
            steering_content: None,
            agent_identity: None,
            tool_policy: ToolPolicy::Unrestricted,
        });

        assert!(
            args.windows(2)
                .any(|pair| pair[0] == "--resume" && pair[1] == "valid-session")
        );
        assert!(
            !args
                .windows(2)
                .any(|pair| pair[0] == "--session-id" && pair[1] == "valid-session"),
            "valid resumes should keep using Claude's --resume path"
        );
    }

    #[test]
    fn claude_cli_args_serialize_xhigh_exactly() {
        let args = build_claude_cli_args(&ClaudeProcessSpawnConfig {
            workspace_root: "/tmp/workspace".to_string(),
            ssh_host: None,
            session_id: None,
            fork_from_session_id: None,
            resume_existing_session: false,
            ephemeral: true,
            model: None,
            effort: Some(ClaudeEffort::XHigh),
            permission_mode: None,
            startup_mcp_config_json: None,
            steering_content: None,
            agent_identity: None,
            tool_policy: ToolPolicy::Unrestricted,
        });

        let effort_pairs = args
            .windows(2)
            .filter(|pair| pair[0] == "--effort")
            .collect::<Vec<_>>();
        assert_eq!(
            effort_pairs.len(),
            1,
            "Claude argv should contain exactly one effort pair"
        );
        assert_eq!(effort_pairs[0][1], "xhigh");
    }

    #[test]
    fn claude_cli_args_omit_unset_effort() {
        let args = build_claude_cli_args(&ClaudeProcessSpawnConfig {
            workspace_root: "/tmp/workspace".to_string(),
            ssh_host: None,
            session_id: None,
            fork_from_session_id: None,
            resume_existing_session: false,
            ephemeral: true,
            model: None,
            effort: None,
            permission_mode: None,
            startup_mcp_config_json: None,
            steering_content: None,
            agent_identity: None,
            tool_policy: ToolPolicy::Unrestricted,
        });

        assert!(!args.iter().any(|arg| arg == "--effort"));
    }

    #[test]
    fn claude_cli_args_fork_uses_parent_resume_without_preseed() {
        let args = build_claude_cli_args(&ClaudeProcessSpawnConfig {
            workspace_root: "/tmp/workspace".to_string(),
            ssh_host: None,
            session_id: None,
            fork_from_session_id: Some("parent-session".to_string()),
            resume_existing_session: true,
            ephemeral: false,
            model: None,
            effort: None,
            permission_mode: None,
            startup_mcp_config_json: None,
            steering_content: None,
            agent_identity: None,
            tool_policy: ToolPolicy::Unrestricted,
        });

        assert!(
            args.windows(2)
                .any(|pair| pair[0] == "--resume" && pair[1] == "parent-session")
        );
        assert!(args.iter().any(|arg| arg == "--fork-session"));
        assert!(
            !args
                .windows(2)
                .any(|pair| pair[0] == "--session-id" && pair[1] == "parent-session"),
            "forks must not pre-seed the parent id as the child session id"
        );
    }

    #[test]
    fn claude_cli_args_recovered_missing_session_starts_fresh() {
        let args = build_claude_cli_args(&ClaudeProcessSpawnConfig {
            workspace_root: "/tmp/workspace".to_string(),
            ssh_host: None,
            session_id: Some("stale-session".to_string()),
            fork_from_session_id: None,
            resume_existing_session: false,
            ephemeral: false,
            model: None,
            effort: None,
            permission_mode: None,
            startup_mcp_config_json: None,
            steering_content: None,
            agent_identity: None,
            tool_policy: ToolPolicy::Unrestricted,
        });

        assert!(
            args.windows(2)
                .any(|pair| pair[0] == "--session-id" && pair[1] == "stale-session"),
            "missing-session recovery should create a fresh Claude CLI session under the same id"
        );
        assert!(
            !args
                .windows(2)
                .any(|pair| pair[0] == "--resume" && pair[1] == "stale-session"),
            "missing-session recovery must not pass the stale id back through --resume"
        );
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

    async fn recv_child_chat_event(
        rx: &mut mpsc::UnboundedReceiver<protocol::ChatEvent>,
        context: &str,
    ) -> protocol::ChatEvent {
        timeout(Duration::from_millis(500), rx.recv())
            .await
            .unwrap_or_else(|_| panic!("{context} should arrive"))
            .unwrap_or_else(|| panic!("{context} channel closed"))
    }

    impl SubAgentEmitter for TestSubAgentEmitter {
        fn on_subagent_spawned(
            &self,
            tool_use_id: String,
            name: String,
            description: String,
            agent_type: String,
            session_id_hint: Option<protocol::SessionId>,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<SubAgentHandle, String>> + Send + '_>,
        > {
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
            Box::pin(async move { Ok(SubAgentHandle { event_tx, agent_id }) })
        }
    }

    fn event_kind(event: &Value) -> Option<&str> {
        event.get("kind").and_then(Value::as_str)
    }

    async fn recv_until_kind(rx: &mut mpsc::UnboundedReceiver<Value>, expected: &str) -> Value {
        timeout(Duration::from_secs(2), async {
            loop {
                let event = rx
                    .recv()
                    .await
                    .expect("Claude event channel should stay open");
                if event_kind(&event) == Some(expected) {
                    return event;
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {expected}"))
    }

    #[tokio::test(flavor = "current_thread")]
    async fn resume_session_missing_history_marks_session_for_fresh_start() {
        let _guard = FAKE_CLAUDE_ENV_LOCK.lock().await;
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        let claude_home = tempfile::tempdir().expect("claude home tempdir");
        let previous_claude_config_dir = std::env::var_os("CLAUDE_CONFIG_DIR");

        unsafe {
            std::env::set_var("CLAUDE_CONFIG_DIR", claude_home.path());
        }

        let (inner, mut rx) =
            make_test_inner_with_workspace(workspace.path().to_string_lossy().to_string());
        let inner = Arc::new(inner);

        inner
            .resume_session("stale-missing-session".to_string())
            .await
            .expect("missing Claude history should recover");

        let session_started = rx.recv().await.expect("session started event");
        assert_eq!(event_kind(&session_started), Some("SessionStarted"));
        let cleared = rx.recv().await.expect("conversation cleared event");
        assert_eq!(event_kind(&cleared), Some("ConversationCleared"));
        let idle = rx.recv().await.expect("idle event");
        assert_eq!(event_kind(&idle), Some("TypingStatusChanged"));
        let warning = rx.recv().await.expect("warning event");
        assert_eq!(event_kind(&warning), Some("MessageAdded"));
        assert_eq!(
            warning
                .get("data")
                .and_then(|data| data.get("sender"))
                .and_then(Value::as_str),
            Some("Warning")
        );
        assert!(
            warning
                .get("data")
                .and_then(|data| data.get("content"))
                .and_then(Value::as_str)
                .is_some_and(|content| content.contains("Starting a fresh Claude session"))
        );

        {
            let state = inner.state.lock().await;
            assert_eq!(state.session_id.as_deref(), Some("stale-missing-session"));
            assert!(
                state.start_session_fresh,
                "next Claude process should use --session-id instead of --resume"
            );
            assert_eq!(state.cumulative_usage, None);
            assert_eq!(state.conversation_bytes_total, 0);
        }

        unsafe {
            if let Some(value) = previous_claude_config_dir {
                std::env::set_var("CLAUDE_CONFIG_DIR", value);
            } else {
                std::env::remove_var("CLAUDE_CONFIG_DIR");
            }
        }
    }

    struct AskAnswerRaceHookGuard;

    impl Drop for AskAnswerRaceHookGuard {
        fn drop(&mut self) {
            *ASK_ANSWER_RACE_HOOK
                .lock()
                .expect("AskUserQuestion answer race hook mutex poisoned") = None;
        }
    }

    fn install_ask_answer_race_hook() -> (
        Arc<tokio::sync::Notify>,
        Arc<tokio::sync::Notify>,
        AskAnswerRaceHookGuard,
    ) {
        let after_write = Arc::new(tokio::sync::Notify::new());
        let resume = Arc::new(tokio::sync::Notify::new());
        *ASK_ANSWER_RACE_HOOK
            .lock()
            .expect("AskUserQuestion answer race hook mutex poisoned") = Some(AskAnswerRaceHook {
            after_write: Arc::clone(&after_write),
            resume: Arc::clone(&resume),
        });
        (after_write, resume, AskAnswerRaceHookGuard)
    }

    #[cfg(unix)]
    fn make_executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(path)
            .unwrap_or_else(|err| panic!("stat fake Claude script {}: {err}", path.display()))
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions)
            .unwrap_or_else(|err| panic!("chmod fake Claude script {}: {err}", path.display()));
    }

    fn stream_end_message(event: &Value) -> &Value {
        event
            .get("data")
            .and_then(|data| data.get("message"))
            .expect("stream end message")
    }

    fn write_fake_exit_plan_mode_script(fake: &Path) {
        std::fs::write(
            fake,
            r##"#!/usr/bin/env python3
import json
import os
import sys

args = sys.argv[1:]
session_id = "fake-exit-plan-session"
if "--session-id" in args:
    session_id = args[args.index("--session-id") + 1]
elif "--resume" in args:
    session_id = args[args.index("--resume") + 1]
log_path = os.environ["TYDE_FAKE_CLAUDE_LOG"]
plan_path = "/repo/.claude/plans/test-plan.md"
plan_content = "# Plan\n\nDo the work.\nRun the tests."

def log(message):
    with open(log_path, "a", encoding="utf-8") as handle:
        handle.write(message + "\n")

def emit(value):
    print(json.dumps(value), flush=True)

log("START " + " ".join(args))
for raw_line in sys.stdin:
    line = raw_line.strip()
    if not line:
        continue
    log("IN " + line)
    value = json.loads(line)
    if value.get("type") == "control_request":
        request = value.get("request", {})
        request_id = value.get("request_id") or request.get("request_id")
        if request.get("subtype") == "initialize":
            emit({
                "type": "control_response",
                "response": {
                    "subtype": "success",
                    "request_id": request_id,
                    "response": {},
                },
            })
        continue
    if value.get("type") == "control_response":
        response = value.get("response", {})
        if response.get("request_id") == "plan-1":
            control = response.get("response", {})
            behavior = control.get("behavior")
            updated = control.get("updatedInput") or {}
            feedback = control.get("message") or control.get("feedback") or ""
            if behavior == "allow":
                text = "plan approved: " + (updated.get("plan") or "")
            else:
                text = "plan rejected: " + feedback
            emit({
                "type": "stream_event",
                "session_id": session_id,
                "event": {
                    "type": "message_start",
                    "message": {"id": "plan-answer-msg", "model": "fake-model", "usage": {"input_tokens": 2}},
                },
            })
            emit({
                "type": "stream_event",
                "session_id": session_id,
                "event": {
                    "type": "content_block_start",
                    "index": 0,
                    "content_block": {"type": "text", "text": ""},
                },
            })
            emit({
                "type": "stream_event",
                "session_id": session_id,
                "event": {
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": {"type": "text_delta", "text": text},
                },
            })
            emit({"type": "stream_event", "session_id": session_id, "event": {"type": "content_block_stop", "index": 0}})
            emit({"type": "stream_event", "session_id": session_id, "event": {"type": "message_stop"}})
            emit({
                "type": "result",
                "subtype": "success",
                "is_error": False,
                "result": text,
                "session_id": session_id,
                "usage": {"input_tokens": 3, "output_tokens": 2, "total_tokens": 5},
            })
        continue
    if value.get("type") == "user":
        emit({
            "type": "system",
            "subtype": "init",
            "session_id": session_id,
            "model": "fake-model",
        })
        emit({
            "type": "stream_event",
            "session_id": session_id,
            "event": {
                "type": "message_start",
                "message": {"id": "plan-msg-1", "model": "fake-model", "usage": {"input_tokens": 1}},
            },
        })
        emit({
            "type": "stream_event",
            "session_id": session_id,
            "event": {
                "type": "content_block_start",
                "index": 0,
                "content_block": {
                    "type": "tool_use",
                    "id": "toolu_write",
                    "name": "Write",
                    "input": {
                        "file_path": plan_path,
                        "content": plan_content,
                    },
                },
            },
        })
        emit({"type": "stream_event", "session_id": session_id, "event": {"type": "content_block_stop", "index": 0}})
        emit({
            "type": "stream_event",
            "session_id": session_id,
            "event": {
                "type": "content_block_start",
                "index": 1,
                "content_block": {
                    "type": "tool_use",
                    "id": "toolu_exit",
                    "name": "ExitPlanMode",
                    "input": {},
                },
            },
        })
        emit({"type": "stream_event", "session_id": session_id, "event": {"type": "content_block_stop", "index": 1}})
        emit({"type": "stream_event", "session_id": session_id, "event": {"type": "message_stop"}})
        emit({
            "type": "control_request",
            "request_id": "plan-1",
            "request": {
                "subtype": "can_use_tool",
                "tool_name": "ExitPlanMode",
                "tool_call_id": "toolu_exit",
                "input": {},
            },
        })
"##,
        )
        .expect("write fake Claude ExitPlanMode script");
        #[cfg(unix)]
        make_executable(fake);
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

    fn stream_end_usage_scope_total_tokens(event: &Value, scope: &str) -> Option<u64> {
        stream_end_message(event)
            .get("token_usage")
            .and_then(|usage| usage.get(scope))
            .and_then(|scope| scope.get("usage"))
            .and_then(|usage| usage.get("total_tokens"))
            .and_then(Value::as_u64)
    }

    fn stream_end_request_total_tokens(event: &Value) -> Option<u64> {
        stream_end_usage_scope_total_tokens(event, "request")
    }

    fn stream_end_turn_total_tokens(event: &Value) -> Option<u64> {
        stream_end_usage_scope_total_tokens(event, "turn")
    }

    fn stream_end_cumulative_total_tokens(event: &Value) -> Option<u64> {
        stream_end_usage_scope_total_tokens(event, "cumulative")
    }

    #[test]
    fn subagent_final_stream_end_uses_child_result_usage() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");

        rt.block_on(async {
            let (child_inner, mut child_rx) = make_test_inner();
            let child_inner = Arc::new(child_inner);
            let message_id = "subagent-toolu_1-seg-1";
            child_inner.emit_stream_start(message_id, Some("claude-test".to_string()));
            let stream_start = child_rx.recv().await.expect("child StreamStart");
            assert_eq!(event_kind(&stream_start), Some("StreamStart"));
            assert_eq!(
                stream_start
                    .get("data")
                    .and_then(|data| data.get("message_id"))
                    .and_then(Value::as_str),
                Some(message_id)
            );
            let (parent_emitter, _parent_rx) = test_parent_emitter();
            let mut stream = SubAgentStream {
                summary: ClaudeStdoutSummary {
                    streamed_text: "child done".to_string(),
                    model: Some("claude-test".to_string()),
                    usage: Some(json!({
                        "input_tokens": 2,
                        "output_tokens": 3,
                        "total_tokens": 5,
                        "cached_prompt_tokens": 0,
                        "cache_creation_input_tokens": 0,
                        "reasoning_tokens": 0
                    })),
                    result_turn_usage: Some(json!({
                        "input_tokens": 7,
                        "output_tokens": 10,
                        "total_tokens": 17,
                        "cached_prompt_tokens": 0,
                        "cache_creation_input_tokens": 0,
                        "reasoning_tokens": 0
                    })),
                    ..ClaudeStdoutSummary::default()
                },
                segment: SegmentState::default(),
                message_id: message_id.to_string(),
                has_explicit_task_prompt: false,
                inner: child_inner,
                parent_tool_use_id: "toolu_1".to_string(),
                agent_id: protocol::AgentId("child-agent".to_string()),
                agent_name: "Child".to_string(),
                parent_emitter,
                last_progress_emit: std::time::Instant::now(),
                execution: SubAgentExecution::Foreground,
            };

            close_current_subagent_phase(&mut stream.summary, &mut stream.segment, &stream.inner);

            let stream_end = child_rx.recv().await.expect("child StreamEnd");
            assert_eq!(event_kind(&stream_end), Some("StreamEnd"));
            assert_eq!(
                stream_end_message(&stream_end)
                    .get("message_id")
                    .and_then(Value::as_str),
                Some(message_id)
            );
            assert_eq!(stream_end_request_total_tokens(&stream_end), Some(5));
            assert_eq!(stream_end_turn_total_tokens(&stream_end), Some(17));
            assert_eq!(stream_end_cumulative_total_tokens(&stream_end), Some(17));
            assert!(
                child_rx.try_recv().is_err(),
                "child phase should not emit an identity error or cancellation"
            );
        });
    }

    #[test]
    fn subagent_final_stream_end_uses_accumulated_request_usage_without_result() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");

        rt.block_on(async {
            let (child_inner, mut child_rx) = make_test_inner();
            let child_inner = Arc::new(child_inner);
            let message_id = "subagent-toolu_1-seg-1";
            child_inner.emit_stream_start(message_id, Some("claude-test".to_string()));
            let stream_start = child_rx.recv().await.expect("child StreamStart");
            assert_eq!(event_kind(&stream_start), Some("StreamStart"));
            assert_eq!(
                stream_start
                    .get("data")
                    .and_then(|data| data.get("message_id"))
                    .and_then(Value::as_str),
                Some(message_id)
            );
            let mut summary = ClaudeStdoutSummary {
                streamed_text: "child done".to_string(),
                model: Some("claude-test".to_string()),
                usage: Some(json!({
                    "input_tokens": 2,
                    "output_tokens": 29,
                    "total_tokens": 31,
                    "cached_prompt_tokens": 0,
                    "cache_creation_input_tokens": 12_969,
                    "reasoning_tokens": 4
                })),
                ..ClaudeStdoutSummary::default()
            };
            let mut segment = SegmentState::default();

            close_current_subagent_phase(&mut summary, &mut segment, &child_inner);

            let stream_end = child_rx.recv().await.expect("child StreamEnd");
            assert_eq!(event_kind(&stream_end), Some("StreamEnd"));
            assert_eq!(
                stream_end_message(&stream_end)
                    .get("message_id")
                    .and_then(Value::as_str),
                Some(message_id)
            );
            assert_eq!(stream_end_request_total_tokens(&stream_end), Some(31));
            assert_eq!(stream_end_turn_total_tokens(&stream_end), Some(31));
            assert_eq!(stream_end_cumulative_total_tokens(&stream_end), Some(31));
            assert_eq!(
                stream_end
                    .get("data")
                    .and_then(|data| data.get("message"))
                    .and_then(|data| data.get("token_usage"))
                    .and_then(|usage| usage.get("turn"))
                    .and_then(|scope| scope.get("usage"))
                    .and_then(|usage| usage.get("cache_creation_input_tokens"))
                    .and_then(Value::as_u64),
                Some(12_969)
            );

            let (events_tx, mut events_rx) = mpsc::unbounded_channel();
            let session_id = Arc::new(std::sync::Mutex::new(None));
            assert!(forward_claude_backend_event(stream_end, &events_tx, &session_id, None).await);
            let event = events_rx.recv().await.expect("adapted child StreamEnd");
            let task_usage = crate::agent::task_usage_scope_from_chat_events_for_test(
                BackendKind::Claude,
                [event],
            );
            let protocol::TaskTokenUsageScope::Known { usage } = task_usage else {
                panic!("Claude child fallback should produce known public task usage");
            };
            assert_eq!(usage.total_tokens, 31);
            assert_eq!(usage.input_tokens, Some(2));
            assert_eq!(usage.output_tokens, Some(29));
            assert_eq!(usage.cached_prompt_tokens, Some(0));
            assert_eq!(usage.cache_creation_input_tokens, Some(12_969));
            assert!(
                child_rx.try_recv().is_err(),
                "child phase should not emit an identity error or cancellation"
            );
        });
    }

    fn usage_only_subagent_stream(
        tool_use_id: &str,
        accumulated: bool,
    ) -> (SubAgentStream, mpsc::UnboundedReceiver<Value>) {
        let (child_inner, child_rx) = make_test_inner();
        let usage = json!({
            "input_tokens": 2,
            "output_tokens": 29,
            "total_tokens": 31,
            "cached_prompt_tokens": 0,
            "cache_creation_input_tokens": 12_969,
            "reasoning_tokens": 0
        });
        let summary = if accumulated {
            ClaudeStdoutSummary {
                accumulated_request_usage: Some(usage),
                ..ClaudeStdoutSummary::default()
            }
        } else {
            ClaudeStdoutSummary {
                usage: Some(usage),
                ..ClaudeStdoutSummary::default()
            }
        };
        let (parent_emitter, _parent_rx) = test_parent_emitter();
        (
            SubAgentStream {
                summary,
                segment: SegmentState {
                    awaiting_stream_start: true,
                    ..SegmentState::default()
                },
                message_id: format!("subagent-{tool_use_id}"),
                has_explicit_task_prompt: false,
                inner: Arc::new(child_inner),
                parent_tool_use_id: tool_use_id.to_string(),
                agent_id: protocol::AgentId(format!("child-{tool_use_id}")),
                agent_name: "Child".to_string(),
                parent_emitter,
                last_progress_emit: std::time::Instant::now(),
                execution: SubAgentExecution::Unknown,
            },
            child_rx,
        )
    }

    async fn assert_usage_only_child_terminal_is_known(
        child_rx: &mut mpsc::UnboundedReceiver<Value>,
    ) {
        let stream_start = child_rx.recv().await.expect("usage-only StreamStart");
        assert_eq!(event_kind(&stream_start), Some("StreamStart"));
        let message_id = stream_start
            .get("data")
            .and_then(|data| data.get("message_id"))
            .and_then(Value::as_str)
            .expect("usage-only stream identity")
            .to_string();

        let stream_end = child_rx.recv().await.expect("usage-only StreamEnd");
        assert_eq!(event_kind(&stream_end), Some("StreamEnd"));
        assert_eq!(
            stream_end_message(&stream_end)
                .get("message_id")
                .and_then(Value::as_str),
            Some(message_id.as_str())
        );
        assert_eq!(stream_end_turn_total_tokens(&stream_end), Some(31));
        assert_eq!(stream_end_cumulative_total_tokens(&stream_end), Some(31));
        assert!(
            child_rx.try_recv().is_err(),
            "usage-only completion should emit exactly one identified stream"
        );

        let (events_tx, mut events_rx) = mpsc::unbounded_channel();
        let session_id = Arc::new(std::sync::Mutex::new(None));
        assert!(forward_claude_backend_event(stream_end, &events_tx, &session_id, None).await);
        let event = events_rx.recv().await.expect("adapted terminal StreamEnd");
        let usage =
            crate::agent::task_usage_scope_from_chat_events_for_test(BackendKind::Claude, [event]);
        assert!(matches!(
            usage,
            protocol::TaskTokenUsageScope::Known { usage }
                if usage.total_tokens == 31
                    && usage.output_tokens == Some(29)
                    && usage.cache_creation_input_tokens == Some(12_969)
        ));
    }

    #[tokio::test]
    async fn task_notification_emits_usage_terminal_without_renderable_payload() {
        for (status, accumulated) in [
            ("completed", true),
            ("failed", false),
            ("error", false),
            ("cancelled", false),
        ] {
            let tool_use_id = format!("toolu-{status}");
            let (stream, mut child_rx) = usage_only_subagent_stream(&tool_use_id, accumulated);
            let mut streams = HashMap::from([(tool_use_id.clone(), stream)]);

            finalize_background_subagent_completion(
                &json!({
                    "type": "system",
                    "subtype": "task_notification",
                    "tool_use_id": tool_use_id,
                    "status": status
                }),
                &mut streams,
            );

            assert!(streams.is_empty());
            assert_usage_only_child_terminal_is_known(&mut child_rx).await;
        }
    }

    #[tokio::test]
    async fn stdout_eof_emits_tool_only_usage_terminal_without_renderable_payload() {
        let (stream, mut child_rx) = usage_only_subagent_stream("toolu-eof", true);

        finalize_subagent_stream(stream);

        assert_usage_only_child_terminal_is_known(&mut child_rx).await;
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
            ClaudeMessageUsage {
                request: phase.usage,
                turn: None,
                cumulative: None,
            },
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

    #[test]
    fn build_stream_json_user_message_allows_image_only_input() {
        let images = vec![make_image("base64-image", "image/png")];
        let payload = build_stream_json_user_message("", &images);
        let content = payload
            .get("message")
            .and_then(|message| message.get("content"))
            .and_then(Value::as_array)
            .expect("content blocks");

        assert_eq!(content.len(), 1);
        assert_eq!(
            content[0].get("type").and_then(Value::as_str),
            Some("image")
        );
        assert_eq!(
            content[0]
                .get("source")
                .and_then(|source| source.get("media_type"))
                .and_then(Value::as_str),
            Some("image/png")
        );
        assert_eq!(
            content[0]
                .get("source")
                .and_then(|source| source.get("data"))
                .and_then(Value::as_str),
            Some("base64-image")
        );
    }

    #[test]
    fn control_request_detects_ask_user_question_for_bridge() {
        let value = json!({
            "type": "control_request",
            "request_id": "req-ask",
            "request": {
                "subtype": "can_use_tool",
                "tool_name": "AskUserQuestion",
                "tool_call_id": "toolu_ask",
                "input": {
                    "questions": [{
                        "question": "Continue?",
                        "options": [{ "id": "yes", "label": "Yes" }],
                    }],
                    "answers": {
                        "question-1": "Yes",
                    },
                },
            },
        });
        let request =
            ask_user_question_control_request(&value).expect("AskUserQuestion bridge request");

        assert_eq!(request.request_id, "req-ask");
        assert_eq!(request.tool_call_id, "toolu_ask");
        assert_eq!(request.tool_name, "AskUserQuestion");
        assert_eq!(
            request
                .input
                .pointer("/questions/0/question")
                .and_then(Value::as_str),
            Some("Continue?")
        );
        let fallback_payload =
            control_response_payload_for_request(&value).expect("fallback response payload");
        assert_eq!(
            fallback_payload
                .pointer("/response/response/behavior")
                .and_then(Value::as_str),
            Some("deny")
        );
    }

    #[test]
    fn control_request_detects_exit_plan_mode_for_bridge() {
        let value = json!({
            "type": "control_request",
            "request_id": "req-plan",
            "request": {
                "subtype": "can_use_tool",
                "tool_name": "ExitPlanMode",
                "tool_call_id": "toolu_exit",
                "input": {
                    "plan": "# Plan\n\nDo the work.",
                    "planFilePath": "/repo/.claude/plans/test.md",
                },
            },
        });
        let request = exit_plan_mode_control_request(&value).expect("ExitPlanMode bridge request");

        assert_eq!(request.request_id, "req-plan");
        assert_eq!(request.tool_call_id, "toolu_exit");
        assert_eq!(request.tool_name, "ExitPlanMode");
        assert_eq!(
            request.input.get("plan").and_then(Value::as_str),
            Some("# Plan\n\nDo the work.")
        );
        let fallback_payload =
            control_response_payload_for_request(&value).expect("fallback response payload");
        assert_eq!(
            fallback_payload
                .pointer("/response/response/behavior")
                .and_then(Value::as_str),
            Some("deny")
        );
    }

    #[test]
    fn control_request_allows_non_interactive_tool_permissions() {
        let input = json!({ "command": "echo ok" });
        let payload = control_response_payload_for_request(&json!({
            "type": "control_request",
            "request_id": "req-bash",
            "request": {
                "subtype": "can_use_tool",
                "tool_name": "Bash",
                "input": input,
            },
        }))
        .expect("control response payload");

        let response = payload
            .get("response")
            .and_then(|response| response.get("response"))
            .expect("nested response");
        assert_eq!(
            response.get("behavior").and_then(Value::as_str),
            Some("allow")
        );
        assert_eq!(response.get("updatedInput"), Some(&input));
    }

    #[test]
    fn exit_plan_mode_response_payloads_match_claude_permissions() {
        let input = json!({"plan": "# Plan"});
        let approve = exit_plan_mode_control_response_payload(
            "req-plan",
            ExitPlanModeDecision::Approve,
            input.clone(),
            "",
        );
        assert_eq!(
            approve
                .pointer("/response/response/behavior")
                .and_then(Value::as_str),
            Some("allow")
        );
        assert_eq!(
            approve.pointer("/response/response/updatedInput"),
            Some(&input)
        );

        let reject = exit_plan_mode_control_response_payload(
            "req-plan",
            ExitPlanModeDecision::Reject,
            input,
            "Needs tests",
        );
        assert_eq!(
            reject
                .pointer("/response/response/behavior")
                .and_then(Value::as_str),
            Some("deny")
        );
        assert_eq!(
            reject
                .pointer("/response/response/message")
                .and_then(Value::as_str),
            Some("Needs tests")
        );
    }

    #[test]
    fn ask_user_question_answer_uses_question_text_key() {
        let input = json!({
            "questions": [{
                "id": "choice",
                "question": "Which choice?",
                "header": "Choice",
                "options": [{"label": "Blue"}, {"label": "Green"}],
            }]
        });

        let updated = ask_user_question_input_with_answer(&input, "Choice: Blue");

        assert_eq!(
            updated
                .pointer("/answers/Which choice?")
                .and_then(Value::as_str),
            Some("Blue")
        );
        assert!(updated.get("answer").is_none());
        assert!(updated.get("answersText").is_none());
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

    #[tokio::test(flavor = "current_thread")]
    async fn fake_persistent_claude_process_handles_interrupt_and_follow_up() {
        let _guard = FAKE_CLAUDE_ENV_LOCK.lock().await;
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        let fake = workspace.path().join("fake-claude.py");
        let log = workspace.path().join("fake-claude.log");
        std::fs::write(
            &fake,
            r#"#!/usr/bin/env python3
import json
import os
import sys

args = sys.argv[1:]
session_id = "fake-session"
if "--session-id" in args:
    session_id = args[args.index("--session-id") + 1]
elif "--resume" in args:
    session_id = args[args.index("--resume") + 1]
log_path = os.environ["TYDE_FAKE_CLAUDE_LOG"]

def log(message):
    with open(log_path, "a", encoding="utf-8") as handle:
        handle.write(message + "\n")

def emit(value):
    print(json.dumps(value), flush=True)

log("START " + " ".join(args))
turn = 0
for raw_line in sys.stdin:
    line = raw_line.strip()
    if not line:
        continue
    log("IN " + line)
    value = json.loads(line)
    if value.get("type") == "control_request":
        request = value.get("request", {})
        request_id = value.get("request_id") or request.get("request_id")
        subtype = request.get("subtype")
        emit({
            "type": "control_response",
            "response": {
                "subtype": "success",
                "request_id": request_id,
                "response": {} if subtype == "initialize" else None,
            },
        })
        if subtype == "interrupt":
            emit({
                "type": "result",
                "subtype": "error_during_execution",
                "is_error": True,
                "result": None,
                "session_id": session_id,
                "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2},
            })
        continue

    if value.get("type") != "user":
        continue
    turn += 1
    if turn == 1:
        emit({
            "type": "system",
            "subtype": "init",
            "session_id": session_id,
            "model": "fake-model",
        })
        emit({
            "type": "stream_event",
            "session_id": session_id,
            "event": {
                "type": "message_start",
                "message": {"id": "fake-msg-1", "model": "fake-model", "usage": {"input_tokens": 1}},
            },
        })
        emit({
            "type": "stream_event",
            "session_id": session_id,
            "event": {
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": "text", "text": "working"},
            },
        })
        continue

    emit({
        "type": "stream_event",
        "session_id": session_id,
        "event": {
            "type": "message_start",
            "message": {"id": "fake-msg-2", "model": "fake-model", "usage": {"input_tokens": 2}},
        },
    })
    emit({
        "type": "stream_event",
        "session_id": session_id,
        "event": {
            "type": "content_block_start",
            "index": 0,
            "content_block": {"type": "text", "text": ""},
        },
    })
    emit({
        "type": "stream_event",
        "session_id": session_id,
        "event": {
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "text_delta", "text": "follow-up ok"},
        },
    })
    emit({"type": "stream_event", "session_id": session_id, "event": {"type": "content_block_stop", "index": 0}})
    emit({"type": "stream_event", "session_id": session_id, "event": {"type": "message_stop"}})
    emit({
        "type": "result",
        "subtype": "success",
        "is_error": False,
        "result": "follow-up ok",
        "session_id": session_id,
        "usage": {"input_tokens": 3, "output_tokens": 2, "total_tokens": 5},
    })
"#,
        )
        .expect("write fake Claude script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&fake)
                .expect("stat fake Claude script")
                .permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&fake, permissions).expect("chmod fake Claude script");
        }

        // SAFETY: this test holds FAKE_CLAUDE_ENV_LOCK for the entire period
        // where the process-global environment points at the fake binary.
        unsafe {
            std::env::set_var(TYDE_CLAUDE_BIN_ENV, &fake);
            std::env::set_var("TYDE_FAKE_CLAUDE_LOG", &log);
        }

        let (session, mut rx) = ClaudeSession::spawn(
            &[workspace.path().to_string_lossy().to_string()],
            None,
            &[],
            None,
            None,
            ToolPolicy::Unrestricted,
            BackendAccessMode::Unrestricted,
        )
        .await
        .expect("spawn fake Claude session");
        session
            .inner
            .ensure_process_ready()
            .await
            .expect("initialize persistent fake Claude process");
        let handle = session.command_handle();
        handle
            .execute(SessionCommand::SendMessage {
                message: "please wait for an interrupt".to_string(),
                images: None,
            })
            .await
            .expect("send first fake turn");

        let first_delta = recv_until_kind(&mut rx, "StreamDelta").await;
        assert_eq!(
            first_delta
                .get("data")
                .and_then(|data| data.get("text"))
                .and_then(Value::as_str),
            Some("working")
        );

        timeout(
            Duration::from_secs(2),
            handle.execute(SessionCommand::CancelConversation),
        )
        .await
        .expect("fake interrupt should quiesce")
        .expect("fake interrupt command should succeed");
        let cancelled = recv_until_kind(&mut rx, "OperationCancelled").await;
        assert_eq!(event_kind(&cancelled), Some("OperationCancelled"));

        handle
            .execute(SessionCommand::SendMessage {
                message: "follow up".to_string(),
                images: None,
            })
            .await
            .expect("send fake follow-up");
        let follow_up_end = recv_until_kind(&mut rx, "StreamEnd").await;
        assert_eq!(
            stream_end_message(&follow_up_end)
                .get("content")
                .and_then(Value::as_str),
            Some("follow-up ok")
        );
        let idle = recv_until_kind(&mut rx, "TypingStatusChanged").await;
        assert_eq!(idle.get("data").and_then(Value::as_bool), Some(false));

        session.shutdown().await;
        // SAFETY: guarded by FAKE_CLAUDE_ENV_LOCK; restore the process-global
        // environment before allowing other tests to run through this section.
        unsafe {
            std::env::remove_var(TYDE_CLAUDE_BIN_ENV);
            std::env::remove_var("TYDE_FAKE_CLAUDE_LOG");
        }

        let log_contents = std::fs::read_to_string(&log).expect("read fake Claude log");
        assert_eq!(log_contents.matches("START ").count(), 1);
        assert!(log_contents.contains("\"subtype\":\"initialize\""));
        assert!(log_contents.contains("\"subtype\":\"interrupt\""));
        assert_eq!(log_contents.matches("\"type\":\"user\"").count(), 2);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fake_resume_missing_history_starts_fresh_and_accepts_follow_up() {
        let _guard = FAKE_CLAUDE_ENV_LOCK.lock().await;
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        let claude_home = tempfile::tempdir().expect("claude home tempdir");
        let fake = workspace.path().join("fake-claude-stale-resume.py");
        let log = workspace.path().join("fake-claude-stale-resume.log");
        std::fs::write(
            &fake,
            r#"#!/usr/bin/env python3
import json
import os
import sys

args = sys.argv[1:]
session_id = "fake-stale-resume-default"
if "--session-id" in args:
    session_id = args[args.index("--session-id") + 1]
elif "--resume" in args:
    session_id = args[args.index("--resume") + 1]
log_path = os.environ["TYDE_FAKE_CLAUDE_LOG"]

def log(message):
    with open(log_path, "a", encoding="utf-8") as handle:
        handle.write(message + "\n")

def emit(value):
    print(json.dumps(value), flush=True)

log("START " + " ".join(args))
for raw_line in sys.stdin:
    line = raw_line.strip()
    if not line:
        continue
    log("IN " + line)
    value = json.loads(line)
    if value.get("type") == "control_request":
        request = value.get("request", {})
        request_id = value.get("request_id") or request.get("request_id")
        if request.get("subtype") == "initialize":
            emit({
                "type": "control_response",
                "response": {
                    "subtype": "success",
                    "request_id": request_id,
                    "response": {},
                },
            })
        continue
    if value.get("type") != "user":
        continue
    emit({
        "type": "system",
        "subtype": "init",
        "session_id": session_id,
        "model": "fake-model",
    })
    emit({
        "type": "stream_event",
        "session_id": session_id,
        "event": {
            "type": "message_start",
            "message": {"id": "fresh-resume-msg", "model": "fake-model", "usage": {"input_tokens": 1}},
        },
    })
    emit({
        "type": "stream_event",
        "session_id": session_id,
        "event": {
            "type": "content_block_start",
            "index": 0,
            "content_block": {"type": "text", "text": ""},
        },
    })
    emit({
        "type": "stream_event",
        "session_id": session_id,
        "event": {
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "text_delta", "text": "fresh follow-up ok"},
        },
    })
    emit({"type": "stream_event", "session_id": session_id, "event": {"type": "content_block_stop", "index": 0}})
    emit({"type": "stream_event", "session_id": session_id, "event": {"type": "message_stop"}})
    emit({
        "type": "result",
        "subtype": "success",
        "is_error": False,
        "result": "fresh follow-up ok",
        "session_id": session_id,
        "usage": {"input_tokens": 2, "output_tokens": 2, "total_tokens": 4},
    })
"#,
        )
        .expect("write fake Claude stale resume script");
        make_executable(&fake);

        let previous_claude_bin = std::env::var_os(TYDE_CLAUDE_BIN_ENV);
        let previous_fake_log = std::env::var_os("TYDE_FAKE_CLAUDE_LOG");
        let previous_claude_config_dir = std::env::var_os("CLAUDE_CONFIG_DIR");
        unsafe {
            std::env::set_var(TYDE_CLAUDE_BIN_ENV, &fake);
            std::env::set_var("TYDE_FAKE_CLAUDE_LOG", &log);
            std::env::set_var("CLAUDE_CONFIG_DIR", claude_home.path());
        }

        let stale_session_id = protocol::SessionId("stale-missing-session".to_string());
        let (backend, mut events) = ClaudeBackend::resume(
            vec![workspace.path().to_string_lossy().to_string()],
            BackendSpawnConfig::default(),
            stale_session_id,
        )
        .await
        .expect("resume should recover missing Claude history");

        let warning = timeout(Duration::from_secs(2), async {
            loop {
                let event = events.recv().await.expect("backend event");
                if let protocol::ChatEvent::MessageAdded(message) = event
                    && matches!(message.sender, protocol::MessageSender::Warning)
                {
                    return message;
                }
            }
        })
        .await
        .expect("missing-session warning should be emitted");
        assert!(warning.content.contains("Starting a fresh Claude session"));

        assert!(
            backend
                .send(protocol::AgentInput::SendMessage(
                    protocol::SendMessagePayload {
                        message: "follow up after stale resume".to_string(),
                        images: None,
                        origin: None,
                        tool_response: None,
                    },
                ))
                .await,
            "backend should remain alive after missing-session recovery"
        );

        timeout(Duration::from_secs(2), async {
            loop {
                let event = events.recv().await.expect("backend event after follow-up");
                if let protocol::ChatEvent::StreamEnd(end) = event
                    && end.message.content == "fresh follow-up ok"
                {
                    break;
                }
            }
        })
        .await
        .expect("fresh follow-up should complete");

        backend.shutdown().await;
        unsafe {
            if let Some(value) = previous_claude_bin {
                std::env::set_var(TYDE_CLAUDE_BIN_ENV, value);
            } else {
                std::env::remove_var(TYDE_CLAUDE_BIN_ENV);
            }
            if let Some(value) = previous_fake_log {
                std::env::set_var("TYDE_FAKE_CLAUDE_LOG", value);
            } else {
                std::env::remove_var("TYDE_FAKE_CLAUDE_LOG");
            }
            if let Some(value) = previous_claude_config_dir {
                std::env::set_var("CLAUDE_CONFIG_DIR", value);
            } else {
                std::env::remove_var("CLAUDE_CONFIG_DIR");
            }
        }

        let log_contents = std::fs::read_to_string(&log).expect("read fake Claude stale log");
        let start_line = log_contents
            .lines()
            .find(|line| line.starts_with("START "))
            .expect("fake Claude process should start after follow-up");
        assert!(
            start_line.contains("--session-id stale-missing-session"),
            "fresh recovery should start Claude with the stale id as a new session: {start_line}"
        );
        assert!(
            !start_line.contains("--resume"),
            "fresh recovery must not ask Claude CLI to resume the missing jsonl: {start_line}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn resumed_backend_finishes_delayed_initialization_before_replay_barrier_starts() {
        let _guard = FAKE_CLAUDE_ENV_LOCK.lock().await;
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        let claude_home = tempfile::tempdir().expect("claude home tempdir");
        let fake = workspace.path().join("fake-claude-delayed-init.py");
        let log = workspace.path().join("fake-claude-delayed-init.log");
        std::fs::write(
            &fake,
            r#"#!/usr/bin/env python3
import json
import os
import sys
import time

log_path = os.environ["TYDE_FAKE_CLAUDE_LOG"]
for raw_line in sys.stdin:
    value = json.loads(raw_line)
    if value.get("type") != "control_request":
        continue
    request = value.get("request", {})
    if request.get("subtype") != "initialize":
        continue
    request_id = value.get("request_id") or request.get("request_id")
    with open(log_path, "a", encoding="utf-8") as handle:
        handle.write("INIT_START\n")
    time.sleep(0.1)
    with open(log_path, "a", encoding="utf-8") as handle:
        handle.write("INIT_DONE\n")
    print(json.dumps({
        "type": "control_response",
        "response": {
            "subtype": "success",
            "request_id": request_id,
            "response": {},
        },
    }), flush=True)
"#,
        )
        .expect("write delayed initialize fake");
        make_executable(&fake);

        let previous_claude_bin = std::env::var_os(TYDE_CLAUDE_BIN_ENV);
        let previous_fake_log = std::env::var_os("TYDE_FAKE_CLAUDE_LOG");
        let previous_claude_config_dir = std::env::var_os("CLAUDE_CONFIG_DIR");
        unsafe {
            std::env::set_var(TYDE_CLAUDE_BIN_ENV, &fake);
            std::env::set_var("TYDE_FAKE_CLAUDE_LOG", &log);
            std::env::set_var("CLAUDE_CONFIG_DIR", claude_home.path());
        }

        let (backend, mut events) = ClaudeBackend::resume(
            vec![workspace.path().to_string_lossy().to_string()],
            BackendSpawnConfig::default(),
            protocol::SessionId("delayed-valid-init".to_string()),
        )
        .await
        .expect("delayed valid initialization should complete");
        let log_contents = std::fs::read_to_string(&log).expect("read delayed init log");
        assert!(log_contents.contains("INIT_START\nINIT_DONE\n"));
        let mut replay_complete = events
            .take_resume_replay_complete()
            .expect("Claude resume replay barrier");
        assert_eq!(replay_complete.try_recv(), Ok(()));

        backend.shutdown().await;
        unsafe {
            if let Some(value) = previous_claude_bin {
                std::env::set_var(TYDE_CLAUDE_BIN_ENV, value);
            } else {
                std::env::remove_var(TYDE_CLAUDE_BIN_ENV);
            }
            if let Some(value) = previous_fake_log {
                std::env::set_var("TYDE_FAKE_CLAUDE_LOG", value);
            } else {
                std::env::remove_var("TYDE_FAKE_CLAUDE_LOG");
            }
            if let Some(value) = previous_claude_config_dir {
                std::env::set_var("CLAUDE_CONFIG_DIR", value);
            } else {
                std::env::remove_var("CLAUDE_CONFIG_DIR");
            }
        }
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn dropping_resumed_backend_startup_cleans_claude_process_guard() {
        let _guard = FAKE_CLAUDE_ENV_LOCK.lock().await;
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        let claude_home = tempfile::tempdir().expect("claude home tempdir");
        let fake = workspace.path().join("fake-claude-stalled-init.py");
        std::fs::write(
            &fake,
            r#"#!/usr/bin/env python3
import json
import sys
import time

for raw_line in sys.stdin:
    value = json.loads(raw_line)
    request = value.get("request", {})
    if value.get("type") == "control_request" and request.get("subtype") == "initialize":
        while True:
            time.sleep(1)
"#,
        )
        .expect("write stalled initialize fake");
        make_executable(&fake);

        let previous_claude_bin = std::env::var_os(TYDE_CLAUDE_BIN_ENV);
        let previous_claude_config_dir = std::env::var_os("CLAUDE_CONFIG_DIR");
        unsafe {
            std::env::set_var(TYDE_CLAUDE_BIN_ENV, &fake);
            std::env::set_var("CLAUDE_CONFIG_DIR", claude_home.path());
        }
        let (spawned_tx, mut spawned_rx) = oneshot::channel();
        *CLAUDE_PROCESS_SPAWN_OBSERVER
            .lock()
            .expect("Claude process spawn observer mutex poisoned") = Some(spawned_tx);
        let mut startup = Box::pin(ClaudeBackend::resume(
            vec![workspace.path().to_string_lossy().to_string()],
            BackendSpawnConfig::default(),
            protocol::SessionId("cancelled-delayed-init".to_owned()),
        ));
        let pid = timeout(FAKE_CLAUDE_EXIT_TIMEOUT, async {
            tokio::select! {
                biased;
                pid = &mut spawned_rx => pid.expect("Claude spawn observer must retain PID sender"),
                _ = startup.as_mut() => {
                    panic!("Claude resume completed before stalled initialization cancellation")
                }
            }
        })
        .await
        .expect("Claude resume must spawn its persistent process")
        .to_string();
        let is_running = |pid: &str| {
            std::process::Command::new("kill")
                .args(["-0", pid])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
        };
        assert!(is_running(&pid), "fixture Claude process must be running");

        drop(startup);

        timeout(FAKE_CLAUDE_EXIT_TIMEOUT, async {
            while is_running(&pid) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("dropping resume startup must trigger the Claude session guard");
        assert!(
            !is_running(&pid),
            "dropping resume startup must trigger the Claude session guard"
        );

        unsafe {
            if let Some(value) = previous_claude_bin {
                std::env::set_var(TYDE_CLAUDE_BIN_ENV, value);
            } else {
                std::env::remove_var(TYDE_CLAUDE_BIN_ENV);
            }
            if let Some(value) = previous_claude_config_dir {
                std::env::set_var("CLAUDE_CONFIG_DIR", value);
            } else {
                std::env::remove_var("CLAUDE_CONFIG_DIR");
            }
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn resumed_backend_surfaces_initialize_failure_before_replay_barrier() {
        let _guard = FAKE_CLAUDE_ENV_LOCK.lock().await;
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        let claude_home = tempfile::tempdir().expect("claude home tempdir");
        let fake = workspace.path().join("fake-claude-init-failure.py");
        std::fs::write(
            &fake,
            r#"#!/usr/bin/env python3
import json
import sys

for raw_line in sys.stdin:
    value = json.loads(raw_line)
    if value.get("type") != "control_request":
        continue
    request = value.get("request", {})
    if request.get("subtype") != "initialize":
        continue
    request_id = value.get("request_id") or request.get("request_id")
    print(json.dumps({
        "type": "control_response",
        "response": {
            "subtype": "error",
            "request_id": request_id,
            "error": {"message": "deliberate initialize rejection"},
        },
    }), flush=True)
"#,
        )
        .expect("write failed initialize fake");
        make_executable(&fake);

        let previous_claude_bin = std::env::var_os(TYDE_CLAUDE_BIN_ENV);
        let previous_claude_config_dir = std::env::var_os("CLAUDE_CONFIG_DIR");
        unsafe {
            std::env::set_var(TYDE_CLAUDE_BIN_ENV, &fake);
            std::env::set_var("CLAUDE_CONFIG_DIR", claude_home.path());
        }

        let error = match ClaudeBackend::resume(
            vec![workspace.path().to_string_lossy().to_string()],
            BackendSpawnConfig::default(),
            protocol::SessionId("failed-valid-init".to_string()),
        )
        .await
        {
            Ok(_) => panic!("initialize rejection must fail resume"),
            Err(error) => error,
        };
        assert!(error.contains("Failed to initialize resumed Claude session"));
        assert!(error.contains("deliberate initialize rejection"));

        unsafe {
            if let Some(value) = previous_claude_bin {
                std::env::set_var(TYDE_CLAUDE_BIN_ENV, value);
            } else {
                std::env::remove_var(TYDE_CLAUDE_BIN_ENV);
            }
            if let Some(value) = previous_claude_config_dir {
                std::env::set_var("CLAUDE_CONFIG_DIR", value);
            } else {
                std::env::remove_var("CLAUDE_CONFIG_DIR");
            }
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fake_exit_plan_mode_control_request_waits_for_approval_message() {
        let _guard = FAKE_CLAUDE_ENV_LOCK.lock().await;
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        let fake = workspace.path().join("fake-claude-exit-plan.py");
        let log = workspace.path().join("fake-claude-exit-plan.log");
        write_fake_exit_plan_mode_script(&fake);

        unsafe {
            std::env::set_var(TYDE_CLAUDE_BIN_ENV, &fake);
            std::env::set_var("TYDE_FAKE_CLAUDE_LOG", &log);
        }

        let (backend, mut events) = ClaudeBackend::spawn(
            vec![workspace.path().to_string_lossy().to_string()],
            BackendSpawnConfig {
                cost_hint: Some(protocol::SpawnCostHint::Low),
                ..BackendSpawnConfig::default()
            },
            protocol::SendMessagePayload {
                message: "plan first".to_string(),
                images: None,
                origin: None,
                tool_response: None,
            },
        )
        .await
        .expect("spawn fake Claude backend");

        let mut saw_pause = false;
        let tool_request = timeout(Duration::from_secs(2), async {
            loop {
                let event = events.recv().await.expect("backend event");
                match event {
                    protocol::ChatEvent::ToolRequest(request)
                        if request.tool_call_id == "toolu_exit" =>
                    {
                        return request;
                    }
                    protocol::ChatEvent::TypingStatusChanged(false) => {
                        saw_pause = true;
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("ExitPlanMode ToolRequest should arrive");

        assert_eq!(tool_request.tool_name, "ExitPlanMode");
        let protocol::ToolRequestType::ExitPlanMode { plan, plan_path } = tool_request.tool_type
        else {
            panic!("expected ExitPlanMode tool request");
        };
        assert_eq!(
            plan.as_deref(),
            Some("# Plan\n\nDo the work.\nRun the tests.")
        );
        assert_eq!(
            plan_path.as_deref(),
            Some("/repo/.claude/plans/test-plan.md")
        );

        timeout(Duration::from_secs(2), async {
            while !saw_pause {
                if let protocol::ChatEvent::TypingStatusChanged(false) =
                    events.recv().await.expect("backend event")
                {
                    saw_pause = true;
                }
            }
        })
        .await
        .expect("ExitPlanMode should pause typing while waiting for approval");

        tokio::time::sleep(Duration::from_millis(200)).await;
        let log_before_approval =
            std::fs::read_to_string(&log).expect("read fake Claude ExitPlanMode log");
        assert!(
            !log_before_approval.contains("\"request_id\":\"plan-1\"")
                && !log_before_approval.contains("\"request_id\": \"plan-1\""),
            "ExitPlanMode control_response must wait for user approval; log={log_before_approval}"
        );

        assert!(
            backend
                .send(protocol::AgentInput::SendMessage(
                    protocol::SendMessagePayload {
                        message: String::new(),
                        images: None,
                        origin: None,
                        tool_response: Some(protocol::SendMessageToolResponse::ExitPlanMode {
                            tool_call_id: "toolu_exit".to_string(),
                            decision: protocol::ExitPlanModeDecision::Approve,
                            feedback: None,
                        }),
                    },
                ))
                .await,
            "backend should accept ExitPlanMode approval"
        );

        let mut saw_completion = false;
        let mut saw_typing_restart = false;
        let mut saw_answer = false;
        timeout(Duration::from_secs(2), async {
            while !(saw_completion && saw_typing_restart && saw_answer) {
                let event = events.recv().await.expect("backend event after approval");
                match event {
                    protocol::ChatEvent::ToolExecutionCompleted(completion)
                        if completion.tool_call_id == "toolu_exit" =>
                    {
                        assert!(completion.success);
                        let protocol::ToolExecutionResult::Other { result } =
                            &completion.tool_result
                        else {
                            panic!("expected Other result");
                        };
                        assert_eq!(
                            result.get("decision").and_then(Value::as_str),
                            Some("approved")
                        );
                        saw_completion = true;
                    }
                    protocol::ChatEvent::TypingStatusChanged(true) => {
                        saw_typing_restart = true;
                    }
                    protocol::ChatEvent::StreamEnd(end)
                        if end.message.content.contains("plan approved: # Plan") =>
                    {
                        saw_answer = true;
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("approval should release Claude control_request");

        backend.shutdown().await;
        unsafe {
            std::env::remove_var(TYDE_CLAUDE_BIN_ENV);
            std::env::remove_var("TYDE_FAKE_CLAUDE_LOG");
        }

        let log_after_approval =
            std::fs::read_to_string(&log).expect("read fake Claude ExitPlanMode log");
        assert!(log_after_approval.contains("\"request_id\":\"plan-1\""));
        assert!(log_after_approval.contains("\"behavior\":\"allow\""));
        assert!(log_after_approval.contains("\"updatedInput\""));
        assert!(log_after_approval.contains("Run the tests."));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fake_exit_plan_mode_reject_sends_deny_feedback() {
        let _guard = FAKE_CLAUDE_ENV_LOCK.lock().await;
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        let fake = workspace.path().join("fake-claude-exit-plan-reject.py");
        let log = workspace.path().join("fake-claude-exit-plan-reject.log");
        write_fake_exit_plan_mode_script(&fake);

        unsafe {
            std::env::set_var(TYDE_CLAUDE_BIN_ENV, &fake);
            std::env::set_var("TYDE_FAKE_CLAUDE_LOG", &log);
        }

        let (backend, mut events) = ClaudeBackend::spawn(
            vec![workspace.path().to_string_lossy().to_string()],
            BackendSpawnConfig {
                cost_hint: Some(protocol::SpawnCostHint::Low),
                ..BackendSpawnConfig::default()
            },
            protocol::SendMessagePayload {
                message: "plan first".to_string(),
                images: None,
                origin: None,
                tool_response: None,
            },
        )
        .await
        .expect("spawn fake Claude backend");

        timeout(Duration::from_secs(2), async {
            loop {
                let event = events.recv().await.expect("backend event");
                if matches!(
                    event,
                    protocol::ChatEvent::ToolRequest(protocol::ToolRequest {
                        ref tool_call_id,
                        ..
                    }) if tool_call_id == "toolu_exit"
                ) {
                    break;
                }
            }
        })
        .await
        .expect("ExitPlanMode ToolRequest should arrive");

        let feedback = "Please add a rollback step.";
        assert!(
            backend
                .send(protocol::AgentInput::SendMessage(
                    protocol::SendMessagePayload {
                        message: String::new(),
                        images: None,
                        origin: None,
                        tool_response: Some(protocol::SendMessageToolResponse::ExitPlanMode {
                            tool_call_id: "toolu_exit".to_string(),
                            decision: protocol::ExitPlanModeDecision::Reject,
                            feedback: Some(feedback.to_string()),
                        }),
                    },
                ))
                .await,
            "backend should accept ExitPlanMode rejection"
        );

        let mut saw_completion = false;
        let mut saw_typing_restart = false;
        let mut saw_answer = false;
        timeout(Duration::from_secs(2), async {
            while !(saw_completion && saw_typing_restart && saw_answer) {
                let event = events.recv().await.expect("backend event after rejection");
                match event {
                    protocol::ChatEvent::ToolExecutionCompleted(completion)
                        if completion.tool_call_id == "toolu_exit" =>
                    {
                        assert!(completion.success);
                        let protocol::ToolExecutionResult::Other { result } =
                            &completion.tool_result
                        else {
                            panic!("expected Other result");
                        };
                        assert_eq!(
                            result.get("decision").and_then(Value::as_str),
                            Some("rejected")
                        );
                        assert_eq!(
                            result.get("feedback").and_then(Value::as_str),
                            Some(feedback)
                        );
                        saw_completion = true;
                    }
                    protocol::ChatEvent::TypingStatusChanged(true) => {
                        saw_typing_restart = true;
                    }
                    protocol::ChatEvent::StreamEnd(end)
                        if end.message.content.contains("plan rejected: Please add") =>
                    {
                        saw_answer = true;
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("rejection should release Claude control_request");

        backend.shutdown().await;
        unsafe {
            std::env::remove_var(TYDE_CLAUDE_BIN_ENV);
            std::env::remove_var("TYDE_FAKE_CLAUDE_LOG");
        }

        let log_after_rejection =
            std::fs::read_to_string(&log).expect("read fake Claude ExitPlanMode log");
        assert!(log_after_rejection.contains("\"behavior\":\"deny\""));
        assert!(log_after_rejection.contains("\"message\":\"Please add a rollback step.\""));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fake_ask_user_question_control_request_waits_for_answer_message() {
        let _guard = FAKE_CLAUDE_ENV_LOCK.lock().await;
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        let fake = workspace.path().join("fake-claude-ask.py");
        let log = workspace.path().join("fake-claude-ask.log");
        std::fs::write(
            &fake,
            r#"#!/usr/bin/env python3
import json
import os
import sys

args = sys.argv[1:]
session_id = "fake-ask-session"
if "--session-id" in args:
    session_id = args[args.index("--session-id") + 1]
elif "--resume" in args:
    session_id = args[args.index("--resume") + 1]
log_path = os.environ["TYDE_FAKE_CLAUDE_LOG"]

def log(message):
    with open(log_path, "a", encoding="utf-8") as handle:
        handle.write(message + "\n")

def emit(value):
    print(json.dumps(value), flush=True)

log("START " + " ".join(args))
for raw_line in sys.stdin:
    line = raw_line.strip()
    if not line:
        continue
    log("IN " + line)
    value = json.loads(line)
    if value.get("type") == "control_request":
        request = value.get("request", {})
        request_id = value.get("request_id") or request.get("request_id")
        if request.get("subtype") == "initialize":
            emit({
                "type": "control_response",
                "response": {
                    "subtype": "success",
                    "request_id": request_id,
                    "response": {},
                },
            })
        continue
    if value.get("type") == "control_response":
        response = value.get("response", {})
        if response.get("request_id") == "ask-1":
            control = response.get("response", {})
            updated = control.get("updatedInput", {})
            answer = updated.get("answers", {}).get("Which choice?", "")
            emit({
                "type": "stream_event",
                "session_id": session_id,
                "event": {
                    "type": "message_start",
                    "message": {"id": "ask-answer-msg", "model": "fake-model", "usage": {"input_tokens": 2}},
                },
            })
            emit({
                "type": "stream_event",
                "session_id": session_id,
                "event": {
                    "type": "content_block_start",
                    "index": 0,
                    "content_block": {"type": "text", "text": ""},
                },
            })
            emit({
                "type": "stream_event",
                "session_id": session_id,
                "event": {
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": {"type": "text_delta", "text": "answer accepted: " + answer},
                },
            })
            emit({"type": "stream_event", "session_id": session_id, "event": {"type": "content_block_stop", "index": 0}})
            emit({"type": "stream_event", "session_id": session_id, "event": {"type": "message_stop"}})
            emit({
                "type": "result",
                "subtype": "success",
                "is_error": False,
                "result": "answer accepted: " + answer,
                "session_id": session_id,
                "usage": {"input_tokens": 3, "output_tokens": 2, "total_tokens": 5},
            })
        continue
    if value.get("type") == "user":
        emit({
            "type": "system",
            "subtype": "init",
            "session_id": session_id,
            "model": "fake-model",
        })
        question_input = {
            "questions": [{
                "id": "choice",
                "question": "Which choice?",
                "header": "Choice",
                "options": [{"label": "Blue"}, {"label": "Green"}],
                "multiSelect": False,
            }],
        }
        emit({
            "type": "stream_event",
            "session_id": session_id,
            "event": {
                "type": "message_start",
                "message": {"id": "ask-msg-1", "model": "fake-model", "usage": {"input_tokens": 1}},
            },
        })
        emit({
            "type": "stream_event",
            "session_id": session_id,
            "event": {
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": "text", "text": "Need a choice before I continue."},
            },
        })
        emit({"type": "stream_event", "session_id": session_id, "event": {"type": "content_block_stop", "index": 0}})
        emit({
            "type": "stream_event",
            "session_id": session_id,
            "event": {
                "type": "content_block_start",
                "index": 1,
                "content_block": {
                    "type": "tool_use",
                    "id": "toolu_ask",
                    "name": "AskUserQuestion",
                    "input": question_input,
                },
            },
        })
        emit({"type": "stream_event", "session_id": session_id, "event": {"type": "content_block_stop", "index": 1}})
        emit({"type": "stream_event", "session_id": session_id, "event": {"type": "message_stop"}})
        emit({
            "type": "control_request",
            "request_id": "ask-1",
            "request": {
                "subtype": "can_use_tool",
                "tool_name": "AskUserQuestion",
                "tool_call_id": "toolu_ask",
                "input": question_input,
            },
        })
	"#,
        )
        .expect("write fake Claude AskUserQuestion script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&fake)
                .expect("stat fake Claude AskUserQuestion script")
                .permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&fake, permissions)
                .expect("chmod fake Claude AskUserQuestion script");
        }

        // SAFETY: this test holds FAKE_CLAUDE_ENV_LOCK for the entire period
        // where the process-global environment points at the fake binary.
        unsafe {
            std::env::set_var(TYDE_CLAUDE_BIN_ENV, &fake);
            std::env::set_var("TYDE_FAKE_CLAUDE_LOG", &log);
        }

        let (backend, mut events) = ClaudeBackend::spawn(
            vec![workspace.path().to_string_lossy().to_string()],
            BackendSpawnConfig {
                cost_hint: Some(protocol::SpawnCostHint::Low),
                ..BackendSpawnConfig::default()
            },
            protocol::SendMessagePayload {
                message: "ask me before continuing".to_string(),
                images: None,
                origin: None,
                tool_response: None,
            },
        )
        .await
        .expect("spawn fake Claude backend");

        let mut events_before_question = Vec::new();
        let tool_request = timeout(Duration::from_secs(2), async {
            loop {
                let event = events.recv().await.expect("backend event");
                events_before_question.push(event.clone());
                if let protocol::ChatEvent::ToolRequest(request) = event {
                    return request;
                }
            }
        })
        .await
        .expect("AskUserQuestion ToolRequest should arrive");
        assert_eq!(tool_request.tool_call_id, "toolu_ask");
        assert_eq!(tool_request.tool_name, "AskUserQuestion");
        assert!(matches!(
            tool_request.tool_type,
            protocol::ToolRequestType::AskUserQuestion { .. }
        ));
        let stream_ends_before_question = events_before_question
            .iter()
            .filter_map(|event| match event {
                protocol::ChatEvent::StreamEnd(end) => Some(end),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            stream_ends_before_question.len(),
            1,
            "AskUserQuestion preamble/tool_use should close exactly one stream phase before the ToolRequest"
        );
        assert_eq!(
            stream_ends_before_question[0].message.content,
            "Need a choice before I continue."
        );
        let tool_call_ids = stream_ends_before_question[0]
            .message
            .tool_calls
            .iter()
            .map(|tool| tool.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(tool_call_ids, vec!["toolu_ask"]);
        assert_eq!(
            events_before_question
                .iter()
                .filter(|event| matches!(event, protocol::ChatEvent::ToolRequest(request) if request.tool_call_id == "toolu_ask"))
                .count(),
            1,
            "AskUserQuestion bridge must not duplicate a streamed ToolRequest"
        );

        tokio::time::sleep(Duration::from_millis(200)).await;
        let log_before_answer =
            std::fs::read_to_string(&log).expect("read fake Claude AskUserQuestion log");
        assert!(
            !log_before_answer.contains("\"request_id\":\"ask-1\"")
                && !log_before_answer.contains("\"request_id\": \"ask-1\""),
            "AskUserQuestion control_response must wait for a user answer; log={log_before_answer}"
        );

        assert!(
            backend
                .send(protocol::AgentInput::SendMessage(
                    protocol::SendMessagePayload {
                        message: "Choice: Blue".to_string(),
                        images: None,
                        origin: None,
                        tool_response: None,
                    },
                ))
                .await,
            "backend should accept AskUserQuestion answer"
        );

        let mut saw_completion = false;
        let mut saw_answer = false;
        let mut saw_idle = false;
        let mut duplicate_tool_requests_after_answer = 0;
        timeout(Duration::from_secs(2), async {
            while !(saw_completion && saw_answer && saw_idle) {
                let event = events.recv().await.expect("backend event after answer");
                match event {
                    protocol::ChatEvent::ToolRequest(request)
                        if request.tool_call_id == "toolu_ask" =>
                    {
                        duplicate_tool_requests_after_answer += 1;
                    }
                    protocol::ChatEvent::ToolExecutionCompleted(completion)
                        if completion.tool_call_id == "toolu_ask" =>
                    {
                        assert!(completion.success);
                        saw_completion = true;
                    }
                    protocol::ChatEvent::StreamEnd(end)
                        if end.message.content.contains("answer accepted: Blue") =>
                    {
                        saw_answer = true;
                    }
                    protocol::ChatEvent::TypingStatusChanged(false) => {
                        saw_idle = true;
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("answer should release Claude control_request");
        assert_eq!(
            duplicate_tool_requests_after_answer, 0,
            "AskUserQuestion answer path must not emit a duplicate ToolRequest"
        );

        backend.shutdown().await;
        // SAFETY: guarded by FAKE_CLAUDE_ENV_LOCK; restore the process-global
        // environment before allowing other tests to run through this section.
        unsafe {
            std::env::remove_var(TYDE_CLAUDE_BIN_ENV);
            std::env::remove_var("TYDE_FAKE_CLAUDE_LOG");
        }

        let log_after_answer =
            std::fs::read_to_string(&log).expect("read fake Claude AskUserQuestion log");
        assert!(log_after_answer.contains("\"request_id\":\"ask-1\""));
        assert!(log_after_answer.contains("Which choice?"));
        assert!(log_after_answer.contains("Blue"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fake_ask_user_question_answer_continuation_waits_for_tool_completion() {
        let _guard = FAKE_CLAUDE_ENV_LOCK.lock().await;
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        let fake = workspace.path().join("fake-claude-ask-race.py");
        let log = workspace.path().join("fake-claude-ask-race.log");
        std::fs::write(
            &fake,
            r#"#!/usr/bin/env python3
import json
import os
import sys

args = sys.argv[1:]
session_id = "fake-ask-race-session"
if "--session-id" in args:
    session_id = args[args.index("--session-id") + 1]
elif "--resume" in args:
    session_id = args[args.index("--resume") + 1]
log_path = os.environ["TYDE_FAKE_CLAUDE_LOG"]

def log(message):
    with open(log_path, "a", encoding="utf-8") as handle:
        handle.write(message + "\n")

def emit(value):
    print(json.dumps(value), flush=True)

log("START " + " ".join(args))
for raw_line in sys.stdin:
    line = raw_line.strip()
    if not line:
        continue
    log("IN " + line)
    value = json.loads(line)
    if value.get("type") == "control_request":
        request = value.get("request", {})
        request_id = value.get("request_id") or request.get("request_id")
        if request.get("subtype") == "initialize":
            emit({
                "type": "control_response",
                "response": {
                    "subtype": "success",
                    "request_id": request_id,
                    "response": {},
                },
            })
        continue
    if value.get("type") == "control_response":
        response = value.get("response", {})
        if response.get("request_id") == "ask-1":
            emit({
                "type": "stream_event",
                "session_id": session_id,
                "event": {
                    "type": "message_start",
                    "message": {"id": "ask-race-answer-msg", "model": "fake-model", "usage": {"input_tokens": 2}},
                },
            })
            emit({
                "type": "stream_event",
                "session_id": session_id,
                "event": {
                    "type": "content_block_start",
                    "index": 0,
                    "content_block": {"type": "text", "text": ""},
                },
            })
            emit({
                "type": "stream_event",
                "session_id": session_id,
                "event": {
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": {"type": "text_delta", "text": "answer accepted after race"},
                },
            })
            emit({"type": "stream_event", "session_id": session_id, "event": {"type": "content_block_stop", "index": 0}})
            emit({"type": "stream_event", "session_id": session_id, "event": {"type": "message_stop"}})
            emit({
                "type": "result",
                "subtype": "success",
                "is_error": False,
                "result": "answer accepted after race",
                "session_id": session_id,
                "usage": {"input_tokens": 3, "output_tokens": 2, "total_tokens": 5},
            })
        continue
    if value.get("type") == "user":
        question_input = {
            "questions": [{
                "id": "choice",
                "question": "Which choice?",
                "header": "Choice",
                "options": [{"label": "Blue"}, {"label": "Green"}],
                "multiSelect": False,
            }],
        }
        emit({
            "type": "system",
            "subtype": "init",
            "session_id": session_id,
            "model": "fake-model",
        })
        emit({
            "type": "stream_event",
            "session_id": session_id,
            "event": {
                "type": "message_start",
                "message": {"id": "ask-race-msg-1", "model": "fake-model", "usage": {"input_tokens": 1}},
            },
        })
        emit({
            "type": "stream_event",
            "session_id": session_id,
            "event": {
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": "text", "text": "Need a choice first."},
            },
        })
        emit({"type": "stream_event", "session_id": session_id, "event": {"type": "content_block_stop", "index": 0}})
        emit({
            "type": "stream_event",
            "session_id": session_id,
            "event": {
                "type": "content_block_start",
                "index": 1,
                "content_block": {
                    "type": "tool_use",
                    "id": "toolu_ask",
                    "name": "AskUserQuestion",
                    "input": question_input,
                },
            },
        })
        emit({"type": "stream_event", "session_id": session_id, "event": {"type": "content_block_stop", "index": 1}})
        emit({"type": "stream_event", "session_id": session_id, "event": {"type": "message_stop"}})
        emit({
            "type": "control_request",
            "request_id": "ask-1",
            "request": {
                "subtype": "can_use_tool",
                "tool_name": "AskUserQuestion",
                "tool_call_id": "toolu_ask",
                "input": question_input,
            },
        })
"#,
        )
        .expect("write fake Claude AskUserQuestion race script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&fake)
                .expect("stat fake Claude AskUserQuestion race script")
                .permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&fake, permissions)
                .expect("chmod fake Claude AskUserQuestion race script");
        }

        // SAFETY: this test holds FAKE_CLAUDE_ENV_LOCK for the entire period
        // where the process-global environment points at the fake binary.
        unsafe {
            std::env::set_var(TYDE_CLAUDE_BIN_ENV, &fake);
            std::env::set_var("TYDE_FAKE_CLAUDE_LOG", &log);
        }

        let (backend, mut events) = ClaudeBackend::spawn(
            vec![workspace.path().to_string_lossy().to_string()],
            BackendSpawnConfig {
                cost_hint: Some(protocol::SpawnCostHint::Low),
                ..BackendSpawnConfig::default()
            },
            protocol::SendMessagePayload {
                message: "ask me before racing".to_string(),
                images: None,
                origin: None,
                tool_response: None,
            },
        )
        .await
        .expect("spawn fake Claude backend");

        timeout(Duration::from_secs(2), async {
            loop {
                let event = events.recv().await.expect("backend event");
                if matches!(
                    event,
                    protocol::ChatEvent::ToolRequest(protocol::ToolRequest {
                        ref tool_call_id,
                        ..
                    }) if tool_call_id == "toolu_ask"
                ) {
                    break;
                }
            }
        })
        .await
        .expect("AskUserQuestion ToolRequest should arrive");

        let (after_write, resume_answer, _hook_guard) = install_ask_answer_race_hook();
        assert!(
            backend
                .send(protocol::AgentInput::SendMessage(
                    protocol::SendMessagePayload {
                        message: "Choice: Blue".to_string(),
                        images: None,
                        origin: None,
                        tool_response: None,
                    },
                ))
                .await,
            "backend should accept AskUserQuestion answer"
        );
        timeout(Duration::from_secs(2), after_write.notified())
            .await
            .expect("answer path should pause after writing control_response");
        tokio::time::sleep(Duration::from_millis(150)).await;
        resume_answer.notify_one();

        let mut events_after_answer = Vec::new();
        timeout(Duration::from_secs(2), async {
            loop {
                let event = events.recv().await.expect("backend event after answer");
                let done = matches!(
                    &event,
                    protocol::ChatEvent::StreamEnd(end)
                        if end.message.content.contains("answer accepted after race")
                );
                events_after_answer.push(event);
                if done {
                    break;
                }
            }
        })
        .await
        .expect("answer continuation should complete");

        let ask_completions = events_after_answer
            .iter()
            .enumerate()
            .filter_map(|(index, event)| match event {
                protocol::ChatEvent::ToolExecutionCompleted(completion)
                    if completion.tool_call_id == "toolu_ask" =>
                {
                    Some((index, completion))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            ask_completions.len(),
            1,
            "AskUserQuestion answer should emit exactly one completion: {events_after_answer:?}"
        );
        assert!(
            ask_completions[0].1.success,
            "AskUserQuestion completion must be successful before continuation"
        );
        let first_continuation_stream_index = events_after_answer
            .iter()
            .position(|event| {
                matches!(
                    event,
                    protocol::ChatEvent::StreamStart(_)
                        | protocol::ChatEvent::StreamDelta(_)
                        | protocol::ChatEvent::StreamEnd(_)
                )
            })
            .expect("continuation stream event should be present");
        assert!(
            ask_completions[0].0 < first_continuation_stream_index,
            "AskUserQuestion completion must precede continuation stream events: {events_after_answer:?}"
        );

        backend.shutdown().await;
        // SAFETY: guarded by FAKE_CLAUDE_ENV_LOCK; restore the process-global
        // environment before allowing other tests to run through this section.
        unsafe {
            std::env::remove_var(TYDE_CLAUDE_BIN_ENV);
            std::env::remove_var("TYDE_FAKE_CLAUDE_LOG");
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fake_claude_exit_without_stdout_result_fails_active_turn() {
        let _guard = FAKE_CLAUDE_ENV_LOCK.lock().await;
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        let fake = workspace.path().join("fake-claude-exit-no-result.py");
        let log = workspace.path().join("fake-claude-exit-no-result.log");
        std::fs::write(
            &fake,
            r#"#!/usr/bin/env python3
import json
import os
import sys

log_path = os.environ["TYDE_FAKE_CLAUDE_LOG"]

def log(message):
    with open(log_path, "a", encoding="utf-8") as handle:
        handle.write(message + "\n")

def emit(value):
    print(json.dumps(value), flush=True)

log("START " + " ".join(sys.argv[1:]))
for raw_line in sys.stdin:
    line = raw_line.strip()
    if not line:
        continue
    log("IN " + line)
    value = json.loads(line)
    if value.get("type") == "control_request":
        request = value.get("request", {})
        request_id = value.get("request_id") or request.get("request_id")
        if request.get("subtype") == "initialize":
            emit({
                "type": "control_response",
                "response": {
                    "subtype": "success",
                    "request_id": request_id,
                    "response": {},
                },
            })
        continue
    if value.get("type") == "user":
        sys.exit(0)
"#,
        )
        .expect("write fake Claude exit script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&fake)
                .expect("stat fake Claude exit script")
                .permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&fake, permissions).expect("chmod fake Claude exit script");
        }

        // SAFETY: this test holds FAKE_CLAUDE_ENV_LOCK for the entire period
        // where the process-global environment points at the fake binary.
        unsafe {
            std::env::set_var(TYDE_CLAUDE_BIN_ENV, &fake);
            std::env::set_var("TYDE_FAKE_CLAUDE_LOG", &log);
        }

        let (session, mut rx) = ClaudeSession::spawn(
            &[workspace.path().to_string_lossy().to_string()],
            None,
            &[],
            None,
            None,
            ToolPolicy::Unrestricted,
            BackendAccessMode::Unrestricted,
        )
        .await
        .expect("spawn fake Claude session");
        let handle = session.command_handle();
        handle
            .execute(SessionCommand::SendMessage {
                message: "exit before result".to_string(),
                images: None,
            })
            .await
            .expect("send fake turn");

        let mut saw_error = false;
        let mut saw_idle = false;
        let mut saw_stream_end = false;
        timeout(FAKE_CLAUDE_EXIT_TIMEOUT, async {
            while !(saw_error && saw_idle) {
                let event = rx.recv().await.expect("backend event");
                match event_kind(&event) {
                    Some("StreamEnd") => {
                        saw_stream_end = true;
                    }
                    Some("Error")
                        if event
                            .get("data")
                            .and_then(Value::as_str)
                            .is_some_and(|message| {
                                message.contains("Claude process exited before returning a result")
                            }) =>
                    {
                        saw_error = true;
                    }
                    Some("TypingStatusChanged")
                        if event.get("data").and_then(Value::as_bool) == Some(false) =>
                    {
                        saw_idle = true;
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("active turn should fail and publish idle after Claude exits");
        assert!(
            saw_stream_end,
            "failed active turn should close the open stream before publishing idle"
        );

        session.shutdown().await;
        // SAFETY: guarded by FAKE_CLAUDE_ENV_LOCK; restore the process-global
        // environment before allowing other tests to run through this section.
        unsafe {
            std::env::remove_var(TYDE_CLAUDE_BIN_ENV);
            std::env::remove_var("TYDE_FAKE_CLAUDE_LOG");
        }

        let log_contents = std::fs::read_to_string(&log).expect("read fake Claude exit log");
        assert!(log_contents.contains("\"subtype\":\"initialize\""));
        assert!(log_contents.contains("\"type\":\"user\""));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fake_claude_exit_while_ask_user_question_pending_fails_tool() {
        let _guard = FAKE_CLAUDE_ENV_LOCK.lock().await;
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        let fake = workspace.path().join("fake-claude-ask-exit.py");
        let log = workspace.path().join("fake-claude-ask-exit.log");
        std::fs::write(
            &fake,
            r#"#!/usr/bin/env python3
import json
import os
import sys

args = sys.argv[1:]
session_id = "fake-ask-exit-session"
if "--session-id" in args:
    session_id = args[args.index("--session-id") + 1]
elif "--resume" in args:
    session_id = args[args.index("--resume") + 1]
log_path = os.environ["TYDE_FAKE_CLAUDE_LOG"]

def log(message):
    with open(log_path, "a", encoding="utf-8") as handle:
        handle.write(message + "\n")

def emit(value):
    print(json.dumps(value), flush=True)

log("START " + " ".join(args))
for raw_line in sys.stdin:
    line = raw_line.strip()
    if not line:
        continue
    log("IN " + line)
    value = json.loads(line)
    if value.get("type") == "control_request":
        request = value.get("request", {})
        request_id = value.get("request_id") or request.get("request_id")
        if request.get("subtype") == "initialize":
            emit({
                "type": "control_response",
                "response": {
                    "subtype": "success",
                    "request_id": request_id,
                    "response": {},
                },
            })
        continue
    if value.get("type") == "user":
        emit({
            "type": "system",
            "subtype": "init",
            "session_id": session_id,
            "model": "fake-model",
        })
        emit({
            "type": "control_request",
            "request_id": "ask-1",
            "request": {
                "subtype": "can_use_tool",
                "tool_name": "AskUserQuestion",
                "tool_call_id": "toolu_ask",
                "input": {
                    "questions": [{
                        "id": "choice",
                        "question": "Which choice?",
                        "options": [{"label": "Blue"}, {"label": "Green"}],
                        "multiSelect": False,
                    }],
                },
            },
        })
        sys.exit(0)
"#,
        )
        .expect("write fake Claude AskUserQuestion exit script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&fake)
                .expect("stat fake Claude AskUserQuestion exit script")
                .permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&fake, permissions)
                .expect("chmod fake Claude AskUserQuestion exit script");
        }

        // SAFETY: this test holds FAKE_CLAUDE_ENV_LOCK for the entire period
        // where the process-global environment points at the fake binary.
        unsafe {
            std::env::set_var(TYDE_CLAUDE_BIN_ENV, &fake);
            std::env::set_var("TYDE_FAKE_CLAUDE_LOG", &log);
        }

        let (backend, mut events) = ClaudeBackend::spawn(
            vec![workspace.path().to_string_lossy().to_string()],
            BackendSpawnConfig {
                cost_hint: Some(protocol::SpawnCostHint::Low),
                ..BackendSpawnConfig::default()
            },
            protocol::SendMessagePayload {
                message: "ask then exit".to_string(),
                images: None,
                origin: None,
                tool_response: None,
            },
        )
        .await
        .expect("spawn fake Claude backend");

        let mut saw_request = false;
        let mut saw_failed_completion = false;
        let mut saw_error = false;
        let mut saw_idle = false;
        timeout(FAKE_CLAUDE_EXIT_TIMEOUT, async {
            while !(saw_request && saw_failed_completion && saw_error && saw_idle) {
                let event = events.recv().await.expect("backend event");
                match event {
                    protocol::ChatEvent::ToolRequest(request)
                        if request.tool_call_id == "toolu_ask" =>
                    {
                        saw_request = true;
                    }
                    protocol::ChatEvent::ToolExecutionCompleted(completion)
                        if completion.tool_call_id == "toolu_ask" =>
                    {
                        assert!(!completion.success);
                        assert!(
                            completion.error.as_deref().is_some_and(
                                |error| error.contains("exited before returning a result")
                            ),
                            "unexpected AskUserQuestion failure: {completion:?}"
                        );
                        saw_failed_completion = true;
                    }
                    protocol::ChatEvent::MessageAdded(message)
                        if matches!(message.sender, protocol::MessageSender::Error)
                            && message
                                .content
                                .contains("Claude process exited before returning a result") =>
                    {
                        saw_error = true;
                    }
                    protocol::ChatEvent::TypingStatusChanged(false) => {
                        saw_idle = true;
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("pending AskUserQuestion should fail when Claude exits");

        backend.shutdown().await;
        // SAFETY: guarded by FAKE_CLAUDE_ENV_LOCK; restore the process-global
        // environment before allowing other tests to run through this section.
        unsafe {
            std::env::remove_var(TYDE_CLAUDE_BIN_ENV);
            std::env::remove_var("TYDE_FAKE_CLAUDE_LOG");
        }
    }

    #[tokio::test]
    async fn ask_user_question_answer_write_failure_emits_failed_completion_only() {
        let (inner, mut rx) = make_test_inner();
        let inner = Arc::new(inner);
        let (outcome_tx, outcome_rx) = oneshot::channel();
        {
            let mut state = inner.state.lock().await;
            state.active_turn = Some(ActiveTurn {
                id: 4242,
                outcome_tx: Some(outcome_tx),
                interrupt_requested: false,
                pending_ask_user_question: Some(PendingAskUserQuestionControl {
                    request_id: "ask-1".to_string(),
                    tool_call_id: "toolu_ask".to_string(),
                    tool_name: "AskUserQuestion".to_string(),
                    input: json!({
                        "questions": [{
                            "id": "choice",
                            "question": "Which choice?",
                            "options": [{"label": "Blue"}],
                        }],
                    }),
                }),
                pending_exit_plan_mode: None,
                quiesced_waiters: Vec::new(),
            });
        }

        let handled = inner
            .answer_pending_ask_user_question("Choice: Blue".to_string(), None)
            .await
            .expect("missing runtime write failure should be scoped to the active turn");
        assert!(
            handled,
            "failed AskUserQuestion answer should consume the input"
        );

        let completion = recv_until_kind(&mut rx, "ToolExecutionCompleted").await;
        assert_eq!(
            completion
                .pointer("/data/tool_call_id")
                .and_then(Value::as_str),
            Some("toolu_ask")
        );
        assert_eq!(
            completion.pointer("/data/success").and_then(Value::as_bool),
            Some(false)
        );
        assert!(
            completion
                .pointer("/data/error")
                .and_then(Value::as_str)
                .is_some_and(|message| message.contains("Failed to send AskUserQuestion answer"))
        );
        while let Ok(event) = rx.try_recv() {
            assert!(
                !(event_kind(&event) == Some("ToolExecutionCompleted")
                    && event.pointer("/data/success").and_then(Value::as_bool) == Some(true)),
                "write failure must not emit a false successful completion: {event}"
            );
        }

        let outcome = timeout(Duration::from_secs(1), outcome_rx)
            .await
            .expect("failed answer write should complete active turn")
            .expect("turn outcome sender should remain alive");
        match outcome {
            TurnOutcome::Failed { error, .. } => {
                assert!(error.contains("Failed to send AskUserQuestion answer"));
            }
            _ => panic!("failed answer write should fail the active turn"),
        }
        let pending = {
            let state = inner.state.lock().await;
            state
                .active_turn
                .as_ref()
                .and_then(|turn| turn.pending_ask_user_question.as_ref())
                .is_some()
        };
        assert!(!pending, "failed write should drain the pending question");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fake_ask_user_question_answer_write_failure_keeps_backend_loop_alive() {
        let _guard = FAKE_CLAUDE_ENV_LOCK.lock().await;
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        let fake = workspace.path().join("fake-claude-ask-write-fail.py");
        let log = workspace.path().join("fake-claude-ask-write-fail.log");
        let state_file = workspace.path().join("fake-claude-ask-write-fail.state");
        std::fs::write(
            &fake,
            r#"#!/usr/bin/env python3
import json
import os
import sys
import time

args = sys.argv[1:]
session_id = "fake-ask-write-fail-session"
if "--session-id" in args:
    session_id = args[args.index("--session-id") + 1]
elif "--resume" in args:
    session_id = args[args.index("--resume") + 1]
log_path = os.environ["TYDE_FAKE_CLAUDE_LOG"]
state_path = os.environ["TYDE_FAKE_CLAUDE_STATE"]

try:
    with open(state_path, "r", encoding="utf-8") as handle:
        process_number = int(handle.read().strip()) + 1
except Exception:
    process_number = 1
with open(state_path, "w", encoding="utf-8") as handle:
    handle.write(str(process_number))

def log(message):
    with open(log_path, "a", encoding="utf-8") as handle:
        handle.write(message + "\n")

def emit(value):
    print(json.dumps(value), flush=True)

log("START " + str(process_number) + " " + " ".join(args))
for raw_line in sys.stdin:
    line = raw_line.strip()
    if not line:
        continue
    log("IN " + line)
    value = json.loads(line)
    if value.get("type") == "control_request":
        request = value.get("request", {})
        request_id = value.get("request_id") or request.get("request_id")
        if request.get("subtype") == "initialize":
            emit({
                "type": "control_response",
                "response": {
                    "subtype": "success",
                    "request_id": request_id,
                    "response": {},
                },
            })
        continue
    if value.get("type") != "user":
        continue
    emit({
        "type": "system",
        "subtype": "init",
        "session_id": session_id,
        "model": "fake-model",
    })
    if process_number == 1:
        emit({
            "type": "control_request",
            "request_id": "ask-1",
            "request": {
                "subtype": "can_use_tool",
                "tool_name": "AskUserQuestion",
                "tool_call_id": "toolu_ask",
                "input": {
                    "questions": [{
                        "id": "choice",
                        "question": "Which choice?",
                        "options": [{"label": "Blue"}, {"label": "Green"}],
                        "multiSelect": False,
                    }],
                },
            },
        })
        os.close(0)
        time.sleep(60)
        sys.exit(0)
    emit({
        "type": "stream_event",
        "session_id": session_id,
        "event": {
            "type": "message_start",
            "message": {"id": "write-fail-followup-msg", "model": "fake-model", "usage": {"input_tokens": 2}},
        },
    })
    emit({
        "type": "stream_event",
        "session_id": session_id,
        "event": {
            "type": "content_block_start",
            "index": 0,
            "content_block": {"type": "text", "text": ""},
        },
    })
    emit({
        "type": "stream_event",
        "session_id": session_id,
        "event": {
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "text_delta", "text": "follow-up after write failure ok"},
        },
    })
    emit({"type": "stream_event", "session_id": session_id, "event": {"type": "content_block_stop", "index": 0}})
    emit({"type": "stream_event", "session_id": session_id, "event": {"type": "message_stop"}})
    emit({
        "type": "result",
        "subtype": "success",
        "is_error": False,
        "result": "follow-up after write failure ok",
        "session_id": session_id,
        "usage": {"input_tokens": 3, "output_tokens": 2, "total_tokens": 5},
    })
"#,
        )
        .expect("write fake Claude AskUserQuestion write failure script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&fake)
                .expect("stat fake Claude AskUserQuestion write failure script")
                .permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&fake, permissions)
                .expect("chmod fake Claude AskUserQuestion write failure script");
        }

        // SAFETY: this test holds FAKE_CLAUDE_ENV_LOCK for the entire period
        // where the process-global environment points at the fake binary.
        unsafe {
            std::env::set_var(TYDE_CLAUDE_BIN_ENV, &fake);
            std::env::set_var("TYDE_FAKE_CLAUDE_LOG", &log);
            std::env::set_var("TYDE_FAKE_CLAUDE_STATE", &state_file);
        }

        let (backend, mut events) = ClaudeBackend::spawn(
            vec![workspace.path().to_string_lossy().to_string()],
            BackendSpawnConfig {
                cost_hint: Some(protocol::SpawnCostHint::Low),
                ..BackendSpawnConfig::default()
            },
            protocol::SendMessagePayload {
                message: "ask then close stdin".to_string(),
                images: None,
                origin: None,
                tool_response: None,
            },
        )
        .await
        .expect("spawn fake Claude backend");

        timeout(Duration::from_secs(2), async {
            loop {
                let event = events.recv().await.expect("backend event");
                if matches!(
                    event,
                    protocol::ChatEvent::ToolRequest(protocol::ToolRequest {
                        ref tool_call_id,
                        ..
                    }) if tool_call_id == "toolu_ask"
                ) {
                    break;
                }
            }
        })
        .await
        .expect("AskUserQuestion ToolRequest should arrive");

        assert!(
            backend
                .send(protocol::AgentInput::SendMessage(
                    protocol::SendMessagePayload {
                        message: "Choice: Blue".to_string(),
                        images: None,
                        origin: None,
                        tool_response: None,
                    },
                ))
                .await,
            "backend should accept answer even when Claude stdin is closed"
        );

        let mut saw_failed_completion = false;
        let mut saw_idle = false;
        timeout(Duration::from_secs(2), async {
            while !(saw_failed_completion && saw_idle) {
                let event = events
                    .recv()
                    .await
                    .expect("backend event after write failure");
                match event {
                    protocol::ChatEvent::ToolExecutionCompleted(completion)
                        if completion.tool_call_id == "toolu_ask" =>
                    {
                        assert!(!completion.success);
                        assert!(
                            completion.error.as_deref().is_some_and(
                                |error| error.contains("Failed to send AskUserQuestion answer")
                            ),
                            "unexpected write-failure completion: {completion:?}"
                        );
                        saw_failed_completion = true;
                    }
                    protocol::ChatEvent::TypingStatusChanged(false) => {
                        saw_idle = true;
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("write failure should fail only the active turn");

        assert!(
            backend
                .send(protocol::AgentInput::SendMessage(
                    protocol::SendMessagePayload {
                        message: "follow up after write failure".to_string(),
                        images: None,
                        origin: None,
                        tool_response: None,
                    },
                ))
                .await,
            "backend loop should remain alive after AskUserQuestion answer write failure"
        );

        timeout(Duration::from_secs(2), async {
            loop {
                let event = events.recv().await.expect("backend event after follow-up");
                if let protocol::ChatEvent::StreamEnd(end) = event
                    && end
                        .message
                        .content
                        .contains("follow-up after write failure ok")
                {
                    break;
                }
            }
        })
        .await
        .expect("follow-up should respawn Claude and complete after write failure");

        backend.shutdown().await;
        // SAFETY: guarded by FAKE_CLAUDE_ENV_LOCK; restore the process-global
        // environment before allowing other tests to run through this section.
        unsafe {
            std::env::remove_var(TYDE_CLAUDE_BIN_ENV);
            std::env::remove_var("TYDE_FAKE_CLAUDE_LOG");
            std::env::remove_var("TYDE_FAKE_CLAUDE_STATE");
        }

        let log_contents =
            std::fs::read_to_string(&log).expect("read fake Claude write failure log");
        assert_eq!(
            log_contents.matches("START ").count(),
            2,
            "follow-up should start a replacement Claude process: {log_contents}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fake_claude_interrupt_while_ask_user_question_pending_quiesces() {
        let _guard = FAKE_CLAUDE_ENV_LOCK.lock().await;
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        let fake = workspace.path().join("fake-claude-ask-interrupt.py");
        let log = workspace.path().join("fake-claude-ask-interrupt.log");
        std::fs::write(
            &fake,
            r#"#!/usr/bin/env python3
import json
import os
import sys

args = sys.argv[1:]
session_id = "fake-ask-interrupt-session"
if "--session-id" in args:
    session_id = args[args.index("--session-id") + 1]
elif "--resume" in args:
    session_id = args[args.index("--resume") + 1]
log_path = os.environ["TYDE_FAKE_CLAUDE_LOG"]

def log(message):
    with open(log_path, "a", encoding="utf-8") as handle:
        handle.write(message + "\n")

def emit(value):
    print(json.dumps(value), flush=True)

log("START " + " ".join(args))
for raw_line in sys.stdin:
    line = raw_line.strip()
    if not line:
        continue
    log("IN " + line)
    value = json.loads(line)
    if value.get("type") == "control_request":
        request = value.get("request", {})
        request_id = value.get("request_id") or request.get("request_id")
        subtype = request.get("subtype")
        if subtype == "initialize":
            emit({
                "type": "control_response",
                "response": {
                    "subtype": "success",
                    "request_id": request_id,
                    "response": {},
                },
            })
        elif subtype == "interrupt":
            emit({
                "type": "control_response",
                "response": {
                    "subtype": "success",
                    "request_id": request_id,
                    "response": None,
                },
            })
            emit({
                "type": "result",
                "subtype": "error_during_execution",
                "is_error": True,
                "result": None,
                "session_id": session_id,
                "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2},
            })
        continue
    if value.get("type") == "user":
        emit({
            "type": "system",
            "subtype": "init",
            "session_id": session_id,
            "model": "fake-model",
        })
        emit({
            "type": "control_request",
            "request_id": "ask-1",
            "request": {
                "subtype": "can_use_tool",
                "tool_name": "AskUserQuestion",
                "tool_call_id": "toolu_ask",
                "input": {
                    "questions": [{
                        "id": "choice",
                        "question": "Which choice?",
                        "options": [{"label": "Blue"}, {"label": "Green"}],
                        "multiSelect": False,
                    }],
                },
            },
        })
"#,
        )
        .expect("write fake Claude AskUserQuestion interrupt script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&fake)
                .expect("stat fake Claude AskUserQuestion interrupt script")
                .permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&fake, permissions)
                .expect("chmod fake Claude AskUserQuestion interrupt script");
        }

        // SAFETY: this test holds FAKE_CLAUDE_ENV_LOCK for the entire period
        // where the process-global environment points at the fake binary.
        unsafe {
            std::env::set_var(TYDE_CLAUDE_BIN_ENV, &fake);
            std::env::set_var("TYDE_FAKE_CLAUDE_LOG", &log);
        }

        let (backend, mut events) = ClaudeBackend::spawn(
            vec![workspace.path().to_string_lossy().to_string()],
            BackendSpawnConfig {
                cost_hint: Some(protocol::SpawnCostHint::Low),
                ..BackendSpawnConfig::default()
            },
            protocol::SendMessagePayload {
                message: "ask then interrupt".to_string(),
                images: None,
                origin: None,
                tool_response: None,
            },
        )
        .await
        .expect("spawn fake Claude backend");

        timeout(Duration::from_secs(2), async {
            loop {
                let event = events.recv().await.expect("backend event");
                if matches!(
                    event,
                    protocol::ChatEvent::ToolRequest(protocol::ToolRequest {
                        ref tool_call_id,
                        ..
                    }) if tool_call_id == "toolu_ask"
                ) {
                    break;
                }
            }
        })
        .await
        .expect("AskUserQuestion ToolRequest should arrive before interrupt");

        let interrupted = timeout(Duration::from_secs(2), backend.interrupt())
            .await
            .expect("interrupt should quiesce");
        assert!(interrupted, "interrupt command should report success");

        let mut saw_failed_completion = false;
        let mut saw_cancelled = false;
        let mut saw_idle = false;
        timeout(Duration::from_secs(2), async {
            while !(saw_failed_completion && saw_cancelled && saw_idle) {
                let event = events.recv().await.expect("backend event after interrupt");
                match event {
                    protocol::ChatEvent::ToolExecutionCompleted(completion)
                        if completion.tool_call_id == "toolu_ask" =>
                    {
                        assert!(!completion.success);
                        assert!(
                            completion
                                .error
                                .as_deref()
                                .is_some_and(|error| error.contains("cancelled")),
                            "unexpected AskUserQuestion cancel failure: {completion:?}"
                        );
                        saw_failed_completion = true;
                    }
                    protocol::ChatEvent::OperationCancelled(_) => {
                        saw_cancelled = true;
                    }
                    protocol::ChatEvent::TypingStatusChanged(false) => {
                        saw_idle = true;
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("interrupt with pending AskUserQuestion should quiesce");

        backend.shutdown().await;
        // SAFETY: guarded by FAKE_CLAUDE_ENV_LOCK; restore the process-global
        // environment before allowing other tests to run through this section.
        unsafe {
            std::env::remove_var(TYDE_CLAUDE_BIN_ENV);
            std::env::remove_var("TYDE_FAKE_CLAUDE_LOG");
        }

        let log_contents = std::fs::read_to_string(&log).expect("read fake Claude interrupt log");
        assert!(log_contents.contains("\"subtype\":\"interrupt\""));
        assert!(
            !log_contents.contains("\"request_id\":\"ask-1\"")
                && !log_contents.contains("\"request_id\": \"ask-1\""),
            "AskUserQuestion should not be answered by interrupt: {log_contents}"
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

    #[test]
    fn parse_claude_session_history_defers_out_of_order_tool_result_until_request() {
        let tool_id = "toolu_out_of_order";
        let assistant_uuid = "assistant-tool-use";
        let contents = format!(
            "{}\n{}\n",
            json!({
                "type": "user",
                "uuid": "tool-result",
                "parentUuid": assistant_uuid,
                "sourceToolAssistantUUID": assistant_uuid,
                "timestamp": "2026-04-26T19:37:44.099Z",
                "message": {
                    "role": "user",
                    "content": [
                        {
                            "type": "tool_result",
                            "tool_use_id": tool_id,
                            "content": [
                                { "type": "text", "text": "Found 1 file\nrelay-protocol/src/lib.rs" }
                            ]
                        }
                    ]
                }
            }),
            json!({
                "type": "assistant",
                "uuid": assistant_uuid,
                "timestamp": "2026-04-26T19:37:44.091Z",
                "message": {
                    "role": "assistant",
                    "model": "claude-opus-4-7",
                    "content": [
                        {
                            "type": "tool_use",
                            "id": tool_id,
                            "name": "Grep",
                            "input": {
                                "pattern": "MobilePairingQrPayload",
                                "path": "/Users/mike/Tyde2/relay-protocol/src",
                                "output_mode": "files_with_matches"
                            }
                        }
                    ]
                }
            })
        );

        let replay = parse_claude_session_history_contents(&contents);
        assert_eq!(replay.len(), 3);

        match &replay[0] {
            ClaudeHistoryReplayItem::Message(message) => {
                let tool_calls = message
                    .get("tool_calls")
                    .and_then(Value::as_array)
                    .expect("message tool_calls");
                assert_eq!(tool_calls.len(), 1);
                assert_eq!(
                    tool_calls[0].get("id").and_then(Value::as_str),
                    Some(tool_id)
                );
            }
            _ => panic!("first replay item should be assistant message"),
        }

        let tool_request = match &replay[1] {
            ClaudeHistoryReplayItem::ToolRequest(tool_call) => tool_call,
            _ => panic!("second replay item should be tool request"),
        };
        assert_eq!(tool_request.id, tool_id);
        assert_eq!(tool_request.name, "Grep");

        let completion = match &replay[2] {
            ClaudeHistoryReplayItem::ToolExecutionCompleted(completion) => completion,
            _ => panic!("third replay item should be tool completion"),
        };
        assert!(completion.success);
        assert_eq!(completion.tool_call_id, tool_id);
        assert_eq!(completion.tool_name, "Grep");
    }

    #[test]
    fn parse_claude_session_history_skips_tool_result_without_matching_request() {
        let contents = format!(
            "{}\n",
            json!({
                "type": "user",
                "message": {
                    "role": "user",
                    "content": [
                        {
                            "type": "tool_result",
                            "tool_use_id": "toolu_missing",
                            "content": [{ "type": "text", "text": "orphaned result" }]
                        }
                    ]
                }
            })
        );

        let replay = parse_claude_session_history_contents(&contents);
        assert!(
            replay.is_empty(),
            "orphaned tool results should not replay as unknown completions"
        );
    }

    #[test]
    fn parse_claude_session_history_auto_closes_abandoned_tool_before_user_message() {
        let tool_id = "toolu_abandoned";
        let contents = format!(
            "{}\n{}\n",
            json!({
                "type": "assistant",
                "message": {
                    "id": "msg_abandoned",
                    "role": "assistant",
                    "content": [
                        {
                            "type": "tool_use",
                            "id": tool_id,
                            "name": "Bash",
                            "input": { "command": "sleep 10" }
                        }
                    ]
                }
            }),
            json!({
                "type": "user",
                "message": {
                    "role": "user",
                    "content": [
                        { "type": "text", "text": "interrupting with a new prompt" }
                    ]
                }
            })
        );

        let replay = parse_claude_session_history_contents(&contents);
        assert_eq!(replay.len(), 4);

        assert!(matches!(&replay[0], ClaudeHistoryReplayItem::Message(_)));
        assert!(matches!(
            &replay[1],
            ClaudeHistoryReplayItem::ToolRequest(_)
        ));
        let completion = match &replay[2] {
            ClaudeHistoryReplayItem::ToolExecutionCompleted(completion) => completion,
            _ => panic!("third replay item should auto-close the abandoned tool"),
        };
        assert_eq!(completion.tool_call_id, tool_id);
        assert_eq!(completion.tool_name, "Bash");
        assert!(!completion.success);
        assert_eq!(
            completion.tool_result.get("kind").and_then(Value::as_str),
            Some("Error")
        );
        assert!(matches!(&replay[3], ClaudeHistoryReplayItem::Message(_)));
    }

    #[test]
    fn parse_claude_session_history_suppresses_late_result_after_auto_close() {
        let tool_id = "toolu_late_result";
        let contents = format!(
            "{}\n{}\n{}\n",
            json!({
                "type": "assistant",
                "message": {
                    "id": "msg_late_result",
                    "role": "assistant",
                    "content": [
                        {
                            "type": "tool_use",
                            "id": tool_id,
                            "name": "Bash",
                            "input": { "command": "sleep 10" }
                        }
                    ]
                }
            }),
            json!({
                "type": "user",
                "message": {
                    "role": "user",
                    "content": [
                        { "type": "text", "text": "new prompt before result arrives" }
                    ]
                }
            }),
            json!({
                "type": "user",
                "message": {
                    "role": "user",
                    "content": [
                        {
                            "type": "tool_result",
                            "tool_use_id": tool_id,
                            "content": "late output"
                        }
                    ]
                }
            })
        );

        let replay = parse_claude_session_history_contents(&contents);
        let completions = replay
            .iter()
            .filter(|item| matches!(item, ClaudeHistoryReplayItem::ToolExecutionCompleted(_)))
            .count();
        assert_eq!(completions, 1, "late completion should be suppressed");

        let completion = replay.iter().find_map(|item| match item {
            ClaudeHistoryReplayItem::ToolExecutionCompleted(completion) => Some(completion),
            _ => None,
        });
        let completion = completion.expect("synthetic completion");
        assert_eq!(completion.tool_call_id, tool_id);
        assert!(!completion.success);
    }

    #[test]
    fn parse_claude_session_history_replays_split_assistant_tools_as_one_turn() {
        let contents = format!(
            "{}\n{}\n{}\n{}\n{}\n{}\n{}\n",
            json!({
                "type": "assistant",
                "message": {
                    "id": "msg_split",
                    "role": "assistant",
                    "content": [
                        { "type": "text", "text": "checking several things" }
                    ]
                }
            }),
            json!({
                "type": "assistant",
                "message": {
                    "id": "msg_split",
                    "role": "assistant",
                    "content": [
                        {
                            "type": "tool_use",
                            "id": "toolu_a",
                            "name": "Bash",
                            "input": { "command": "git log --oneline -1" }
                        }
                    ]
                }
            }),
            json!({
                "type": "assistant",
                "message": {
                    "id": "msg_split",
                    "role": "assistant",
                    "content": [
                        {
                            "type": "tool_use",
                            "id": "toolu_b",
                            "name": "Bash",
                            "input": { "command": "git status --short" }
                        }
                    ]
                }
            }),
            json!({
                "type": "user",
                "message": {
                    "role": "user",
                    "content": [
                        { "type": "tool_result", "tool_use_id": "toolu_b", "content": "clean" }
                    ]
                }
            }),
            json!({
                "type": "assistant",
                "message": {
                    "id": "msg_split",
                    "role": "assistant",
                    "content": [
                        {
                            "type": "tool_use",
                            "id": "toolu_c",
                            "name": "Grep",
                            "input": { "pattern": "needle", "path": "/tmp" }
                        }
                    ]
                }
            }),
            json!({
                "type": "user",
                "message": {
                    "role": "user",
                    "content": [
                        { "type": "tool_result", "tool_use_id": "toolu_a", "content": "abc123" }
                    ]
                }
            }),
            json!({
                "type": "user",
                "message": {
                    "role": "user",
                    "content": [
                        { "type": "tool_result", "tool_use_id": "toolu_c", "content": "match" }
                    ]
                }
            })
        );

        let replay = parse_claude_session_history_contents(&contents);
        let messages = replay
            .iter()
            .filter(|item| matches!(item, ClaudeHistoryReplayItem::Message(_)))
            .count();
        let requests = replay
            .iter()
            .filter(|item| matches!(item, ClaudeHistoryReplayItem::ToolRequest(_)))
            .count();
        let completions = replay
            .iter()
            .filter_map(|item| match item {
                ClaudeHistoryReplayItem::ToolExecutionCompleted(completion) => Some(completion),
                _ => None,
            })
            .collect::<Vec<_>>();

        assert_eq!(messages, 1);
        assert_eq!(requests, 3);
        assert_eq!(completions.len(), 3);
        assert!(
            completions.iter().all(|completion| completion.success),
            "split same-message tool calls should not be auto-closed"
        );
    }

    #[tokio::test]
    async fn top_level_assistant_boundaries_emit_separate_stream_ends_without_raw_cumulative_usage()
    {
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
        assert_eq!(stream_end_request_total_tokens(&first_end), Some(120));
        assert_eq!(stream_end_turn_total_tokens(&first_end), None);

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
        assert_eq!(stream_end_request_total_tokens(&second_end), Some(300));
        assert_eq!(stream_end_turn_total_tokens(&second_end), None);
    }

    #[tokio::test]
    async fn terminal_context_breakdown_uses_per_call_usage_not_turn_delta() {
        // Regression: the Context Usage bar derives from `context_breakdown`.
        // The breakdown must reflect the last API call's prompt footprint
        // (`summary.usage`, bounded by the context window), NOT `turn_usage`
        // — the per-turn delta of Claude's session-cumulative counter, which
        // sums input tokens across every API call in a multi-step turn and
        // overflows the window (e.g. 14.5M against a 1M window pins the bar
        // to 100%).
        let (inner, mut rx) = make_test_inner();
        // Last API call actually consumed 250 input tokens (the context fill).
        let mut summary = ClaudeStdoutSummary {
            streamed_text: "Final answer".to_string(),
            model: Some("claude-opus-4-6".to_string()),
            usage: Some(json!({
                "input_tokens": 250,
                "output_tokens": 50,
                "total_tokens": 300,
            })),
            ..Default::default()
        };

        inner.emit_stream_start("claude-msg-1", None);
        assert_eq!(
            event_kind(&rx.recv().await.expect("stream start")),
            Some("StreamStart")
        );

        // Per-turn delta summed across the whole agentic turn — must NOT drive
        // the breakdown.
        let turn_usage = ClaudeTurnUsage {
            turn: json!({
                "input_tokens": 14_000_000,
                "output_tokens": 500_000,
                "total_tokens": 14_500_000,
            }),
            cumulative: Some(json!({
                "input_tokens": 14_000_000,
                "output_tokens": 500_000,
                "total_tokens": 14_500_000,
            })),
        };
        inner
            .emit_terminal_phase_or_placeholder(
                &mut summary,
                0,
                Some(1_000_000),
                None,
                Some(turn_usage),
            )
            .await;

        let end = rx.recv().await.expect("stream end");
        assert_eq!(event_kind(&end), Some("StreamEnd"));
        let message = stream_end_message(&end);

        // Context breakdown reflects the per-call usage (250), not the 14M delta.
        let breakdown_input = message
            .get("context_breakdown")
            .and_then(|bd| bd.get("input_tokens"))
            .and_then(Value::as_u64);
        assert_eq!(breakdown_input, Some(250));

        // The turn scope still carries the per-turn delta.
        assert_eq!(stream_end_request_total_tokens(&end), Some(300));
        assert_eq!(stream_end_turn_total_tokens(&end), Some(14_500_000));
        assert_eq!(stream_end_cumulative_total_tokens(&end), Some(14_500_000));
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
        assert_eq!(stream_end_request_total_tokens(&stream_end), Some(80));
        assert_eq!(stream_end_turn_total_tokens(&stream_end), None);

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
    async fn unresolved_streamed_tool_request_is_auto_closed_before_next_stream_start() {
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
                        "model": "claude-opus-4-6"
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
                        "id": "toolu_orphan",
                        "name": "Grep",
                        "input": {
                            "pattern": "needle",
                            "path": "/tmp"
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
                    "type": "message_stop"
                }
            }),
            &mut summary,
            &mut segment,
            &inner,
            &base_id,
            &mut current_id,
        );

        let stream_end = rx.recv().await.expect("stream end for tool phase");
        assert_eq!(event_kind(&stream_end), Some("StreamEnd"));
        assert_eq!(
            stream_end_tool_call_ids(&stream_end),
            vec!["toolu_orphan".to_string()]
        );

        let tool_request = rx.recv().await.expect("tool request");
        assert_eq!(event_kind(&tool_request), Some("ToolRequest"));
        assert_eq!(
            tool_request
                .get("data")
                .and_then(|data| data.get("tool_call_id"))
                .and_then(Value::as_str),
            Some("toolu_orphan")
        );

        consume_claude_stream_value(
            &json!({
                "type": "stream_event",
                "event": {
                    "type": "message_start",
                    "message": {
                        "id": "assistant-msg-2",
                        "model": "claude-opus-4-6"
                    }
                }
            }),
            &mut summary,
            &mut segment,
            &inner,
            &base_id,
            &mut current_id,
        );

        let auto_completion = rx
            .recv()
            .await
            .expect("synthetic tool completion before next stream");
        assert_eq!(event_kind(&auto_completion), Some("ToolExecutionCompleted"));
        assert_eq!(
            auto_completion
                .get("data")
                .and_then(|data| data.get("tool_call_id"))
                .and_then(Value::as_str),
            Some("toolu_orphan")
        );
        assert_eq!(
            auto_completion
                .get("data")
                .and_then(|data| data.get("success"))
                .and_then(Value::as_bool),
            Some(false)
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

        consume_claude_stream_value(
            &json!({
                "type": "user",
                "message": {
                    "role": "user",
                    "content": [
                        {
                            "type": "tool_result",
                            "tool_use_id": "toolu_orphan",
                            "content": "late result"
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
        assert!(
            rx.try_recv().is_err(),
            "late real completion after synthetic auto-close should be suppressed"
        );
    }

    #[tokio::test]
    async fn wrapped_event_envelope_emits_reasoning_delta() {
        let (inner, mut rx) = make_test_inner();
        let mut summary = ClaudeStdoutSummary::default();
        let mut segment = SegmentState::default();
        let base_id = "claude-msg-1".to_string();
        let mut current_id = base_id.clone();
        inner.emit_stream_start(&base_id, None);
        let stream_start = rx.recv().await.expect("turn StreamStart");
        assert_eq!(event_kind(&stream_start), Some("StreamStart"));
        assert_eq!(
            stream_start
                .get("data")
                .and_then(|data| data.get("message_id"))
                .and_then(Value::as_str),
            Some(base_id.as_str())
        );

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
        assert_eq!(
            event
                .get("data")
                .and_then(|data| data.get("message_id"))
                .and_then(Value::as_str),
            Some(base_id.as_str())
        );
        assert!(
            rx.try_recv().is_err(),
            "wrapped reasoning should not emit an identity error or cancellation"
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

    // ---- Workflow task-frame reducer (fixtures from a live CLI probe) ----

    fn recv_tool_progress(rx: &mut mpsc::UnboundedReceiver<Value>) -> Vec<WorkflowRunState> {
        let mut snapshots = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if event.get("kind").and_then(Value::as_str) != Some("ToolProgress") {
                continue;
            }
            let data: ToolProgressData =
                serde_json::from_value(event.get("data").cloned().unwrap_or(Value::Null))
                    .expect("ToolProgress payload parses");
            assert_eq!(data.tool_call_id, "toolu_wf");
            assert_eq!(data.tool_name, "Workflow");
            let ToolProgressUpdate::Workflow(state) = data.update else {
                panic!("expected Workflow update");
            };
            snapshots.push(state);
        }
        snapshots
    }

    fn workflow_started_frame() -> Value {
        json!({
            "type": "system",
            "subtype": "task_started",
            "task_id": "task-wf",
            "tool_use_id": "toolu_wf",
            "description": "Probe: two agents reply hello",
            "task_type": "local_workflow",
            "workflow_name": "wfprobe",
            "prompt": "export const meta = { name: 'wfprobe' }",
        })
    }

    fn workflow_progress_frame(deltas: Value, total_tokens: u64) -> Value {
        json!({
            "type": "system",
            "subtype": "task_progress",
            "task_id": "task-wf",
            "tool_use_id": "toolu_wf",
            "summary": "Probe: two agents reply hello",
            "usage": {"total_tokens": total_tokens, "tool_uses": 0, "duration_ms": 1586},
            "workflow_progress": deltas,
        })
    }

    fn agent_delta(index: u64, label: &str, state: &str) -> Value {
        json!({
            "type": "workflow_agent",
            "index": index,
            "label": label,
            "phaseIndex": 1,
            "phaseTitle": "Probe",
            "agentId": format!("a{index}"),
            "model": "claude-opus-4-8[1m]",
            "state": state,
            "attempt": 1,
            "promptPreview": "Reply with exactly the word hello",
            "tokens": 6539,
            "toolCalls": 0,
            "durationMs": 1562,
            "resultPreview": "hello",
        })
    }

    #[test]
    fn workflow_task_frames_reduce_to_snapshots() {
        let (inner, mut rx) = make_test_inner();
        let mut runs: HashMap<String, WorkflowRunEntry> = HashMap::new();

        assert!(handle_workflow_task_frame(
            &workflow_started_frame(),
            &mut runs,
            &inner.emitter,
        ));
        assert!(handle_workflow_task_frame(
            &workflow_progress_frame(
                json!([
                    agent_delta(1, "probe-1", "start"),
                    agent_delta(2, "probe-2", "start")
                ]),
                6539,
            ),
            &mut runs,
            &inner.emitter,
        ));
        // Progress without a state transition inside the throttle window
        // must not emit.
        assert!(handle_workflow_task_frame(
            &workflow_progress_frame(json!([agent_delta(2, "probe-2", "progress")]), 9000),
            &mut runs,
            &inner.emitter,
        ));
        assert!(handle_workflow_task_frame(
            &workflow_progress_frame(json!([agent_delta(2, "probe-2", "done")]), 10000),
            &mut runs,
            &inner.emitter,
        ));
        assert!(handle_workflow_task_frame(
            &workflow_progress_frame(json!([agent_delta(1, "probe-1", "done")]), 13078),
            &mut runs,
            &inner.emitter,
        ));
        assert!(handle_workflow_task_frame(
            &json!({
                "type": "system",
                "subtype": "task_notification",
                "task_id": "task-wf",
                "tool_use_id": "toolu_wf",
                "status": "completed",
                "summary": "Dynamic workflow completed",
            }),
            &mut runs,
            &inner.emitter,
        ));
        assert!(runs.is_empty(), "run is dropped after its notification");

        let snapshots = recv_tool_progress(&mut rx);
        // started + both-start + probe-2-done + probe-1-done + notification.
        assert_eq!(snapshots.len(), 5);

        let first = &snapshots[0];
        assert_eq!(first.workflow_name, "wfprobe");
        assert_eq!(
            first.script.as_deref(),
            Some("export const meta = { name: 'wfprobe' }")
        );
        assert_eq!(first.status, WorkflowRunStatus::Running);
        assert!(first.agents.is_empty());

        let last = snapshots.last().unwrap();
        assert_eq!(last.status, WorkflowRunStatus::Completed);
        assert_eq!(last.summary.as_deref(), Some("Dynamic workflow completed"));
        assert_eq!(last.total_tokens, 13078);
        assert_eq!(last.agents.len(), 2);
        assert_eq!(last.agents[0].index, 1);
        assert_eq!(last.agents[0].label, "probe-1");
        assert_eq!(last.agents[0].state, WorkflowAgentStatus::Done);
        assert_eq!(last.agents[0].phase_title.as_deref(), Some("Probe"));
        assert_eq!(last.agents[0].result_preview.as_deref(), Some("hello"));
        assert_eq!(last.agents[1].state, WorkflowAgentStatus::Done);
    }

    #[test]
    fn workflow_malformed_deltas_and_unknown_states_are_tolerated() {
        let (inner, mut rx) = make_test_inner();
        let mut runs: HashMap<String, WorkflowRunEntry> = HashMap::new();

        handle_workflow_task_frame(&workflow_started_frame(), &mut runs, &inner.emitter);
        assert!(handle_workflow_task_frame(
            &workflow_progress_frame(
                json!([
                    {"type": "workflow_agent"},               // no index
                    {"type": "something_else", "index": 9},   // not an agent
                    "not even an object",
                    agent_delta(1, "probe-1", "some_future_state"),
                ]),
                100,
            ),
            &mut runs,
            &inner.emitter,
        ));

        let snapshots = recv_tool_progress(&mut rx);
        let last = snapshots.last().unwrap();
        assert_eq!(last.agents.len(), 1);
        assert_eq!(last.agents[0].state, WorkflowAgentStatus::Unknown);
    }

    #[test]
    fn non_workflow_task_frames_fall_through() {
        let (inner, mut rx) = make_test_inner();
        let mut runs: HashMap<String, WorkflowRunEntry> = HashMap::new();

        // A local_agent task_started belongs to the subagent path.
        assert!(!handle_workflow_task_frame(
            &json!({
                "type": "system",
                "subtype": "task_started",
                "task_id": "task-agent",
                "tool_use_id": "toolu_task",
                "task_type": "local_agent",
            }),
            &mut runs,
            &inner.emitter,
        ));
        // Progress for an untracked task is not ours either.
        assert!(!handle_workflow_task_frame(
            &workflow_progress_frame(json!([]), 1),
            &mut runs,
            &inner.emitter,
        ));
        assert!(recv_tool_progress(&mut rx).is_empty());
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
        let (parent_emitter, _parent_rx) = test_parent_emitter();
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
            &parent_emitter,
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
        let prompt_event = recv_child_chat_event(&mut child_events, "task prompt event").await;
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

        let stream_start = recv_child_chat_event(&mut child_events, "child stream start").await;
        let protocol::ChatEvent::StreamStart(start) = stream_start else {
            panic!("expected child StreamStart event");
        };
        assert!(
            start.message_id.is_some(),
            "child stream start should carry a message id"
        );

        let stream_delta = recv_child_chat_event(&mut child_events, "child stream delta").await;
        let protocol::ChatEvent::StreamDelta(delta) = stream_delta else {
            panic!("expected child StreamDelta event");
        };
        assert_eq!(delta.text, "child says hello");

        let mut pending_prompts = HashMap::new();
        detect_subagent_spawns(
            &json!({
                "type": "assistant", "message": { "content": [{
                    "type": "tool_use", "id": "toolu_123", "name": "Agent",
                    "input": {
                        "prompt": "Trace the end-to-end flow",
                        "run_in_background": false
                    }
                }]}
            }),
            &emitter,
            &parent_emitter,
            &mut streams,
            &mut pending_prompts,
        )
        .await;

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

        let stream_end = recv_child_chat_event(&mut child_events, "child stream end").await;
        let protocol::ChatEvent::StreamEnd(end) = stream_end else {
            panic!("expected child StreamEnd event");
        };
        assert_eq!(end.message.content, "child says hello");

        assert!(
            streams.is_empty(),
            "sub-agent stream should be removed after tool_result completion"
        );
    }

    #[test]
    fn is_cli_turn_start_event_classifies_frames() {
        // Fresh turn content opens a CLI-initiated turn.
        assert!(is_cli_turn_start_event(&json!({"type": "assistant"})));
        assert!(is_cli_turn_start_event(
            &json!({"type": "system", "subtype": "init"})
        ));
        assert!(is_cli_turn_start_event(&json!({"type": "stream_event"})));
        assert!(is_cli_turn_start_event(&json!({"type": "message_start"})));

        // Terminal / non-content frames must NOT spawn an empty turn.
        assert!(!is_cli_turn_start_event(&json!({"type": "result"})));
        assert!(!is_cli_turn_start_event(&json!({"type": "user"})));
        assert!(!is_cli_turn_start_event(
            &json!({"type": "system", "subtype": "task_notification"})
        ));
        assert!(!is_cli_turn_start_event(
            &json!({"type": "rate_limit_event"})
        ));
    }

    #[tokio::test]
    async fn background_subagent_survives_launch_tool_result_until_task_notification() {
        // Reproduces the real background-agent event ordering: the parent's
        // synthetic launch tool_result arrives BEFORE the sub-agent's own
        // output, so tearing the stream down on that tool_result drops the
        // real sub-agent output. The stream must instead survive until the
        // `task_notification` completion frame.
        let emitter = TestSubAgentEmitter::default();
        let (parent_emitter, _parent_rx) = test_parent_emitter();
        let mut streams = HashMap::new();
        let mut pending_prompts = HashMap::new();

        // Parent spawns a background Agent (run_in_background: true).
        detect_subagent_spawns(
            &json!({
                "type": "assistant",
                "message": {
                    "role": "assistant",
                    "content": [{
                        "type": "tool_use",
                        "id": "toolu_bg",
                        "name": "Agent",
                        "input": {
                            "description": "Compute 2+2",
                            "prompt": "Compute 2+2",
                            "subagent_type": "general-purpose",
                            "run_in_background": true
                        }
                    }]
                }
            }),
            &emitter,
            &parent_emitter,
            &mut streams,
            &mut pending_prompts,
        )
        .await;

        assert!(streams.contains_key("toolu_bg"));
        assert!(
            streams.get("toolu_bg").expect("stream").execution == SubAgentExecution::Background,
            "run_in_background tool_use must mark the stream as background"
        );

        // The synthetic launch tool_result arrives immediately — it must NOT
        // finalize a background stream.
        detect_subagent_completions(
            &json!({
                "type": "user",
                "message": {
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": "toolu_bg",
                        "is_error": false,
                        "content": "The agent is working in the background. You will be notified automatically when it completes."
                    }]
                }
            }),
            &mut streams,
        )
        .await;
        assert!(
            streams.contains_key("toolu_bg"),
            "background sub-agent stream must survive its launch tool_result"
        );

        // The sub-agent's real output streams afterwards and still routes.
        let stream = streams.get_mut("toolu_bg").expect("stream");
        consume_subagent_event(
            stream,
            &json!({
                "type": "content_block_start",
                "parent_tool_use_id": "toolu_bg",
                "index": 0,
                "content_block": { "type": "text", "text": "4" }
            }),
        );

        // The task_notification completion frame finalizes the stream.
        finalize_background_subagent_completion(
            &json!({
                "type": "system",
                "subtype": "task_notification",
                "tool_use_id": "toolu_bg",
                "status": "completed"
            }),
            &mut streams,
        );
        assert!(
            streams.is_empty(),
            "background sub-agent stream should be removed on task_notification"
        );
    }

    #[tokio::test]
    async fn omitted_background_flag_uses_cli_task_lifecycle() {
        let emitter = TestSubAgentEmitter::default();
        let (parent_emitter, _parent_rx) = test_parent_emitter();
        let mut streams = HashMap::new();
        let mut pending_prompts = HashMap::new();
        let spawn = json!({
            "type": "assistant",
            "message": { "content": [{
                "type": "tool_use", "id": "toolu_default", "name": "Agent",
                "input": { "description": "Inspect", "prompt": "Inspect code" }
            }]}
        });

        detect_subagent_spawns(
            &spawn,
            &emitter,
            &parent_emitter,
            &mut streams,
            &mut pending_prompts,
        )
        .await;
        assert_eq!(
            streams.get("toolu_default").expect("stream").execution,
            SubAgentExecution::Unknown
        );

        detect_subagent_task_system_spawns(
            &json!({
                "type": "system", "subtype": "task_started",
                "task_type": "local_agent", "tool_use_id": "toolu_default"
            }),
            &emitter,
            &parent_emitter,
            &mut streams,
        )
        .await;
        assert_eq!(
            streams.get("toolu_default").expect("stream").execution,
            SubAgentExecution::Unknown
        );

        detect_subagent_completions(
            &json!({
                "type": "user", "message": { "content": [{
                    "type": "tool_result", "tool_use_id": "toolu_default",
                    "content": "The agent is working in the background. You will be notified automatically when it completes."
                }]}
            }),
            &mut streams,
        )
        .await;
        assert!(streams.contains_key("toolu_default"));

        let stream = streams.get_mut("toolu_default").expect("stream");
        consume_subagent_event(
            stream,
            &json!({
                "type": "content_block_start", "parent_tool_use_id": "toolu_default",
                "index": 0,
                "content_block": {"type": "text", "text": "tool-first complete"}
            }),
        );
        consume_subagent_event(
            stream,
            &json!({
                "type": "result", "parent_tool_use_id": "toolu_default",
                "result": "tool-first complete",
                "usage": {"input_tokens": 12, "output_tokens": 3}
            }),
        );
        finalize_background_subagent_completion(
            &json!({
                "type": "system", "subtype": "task_notification",
                "tool_use_id": "toolu_default", "status": "completed"
            }),
            &mut streams,
        );
        assert!(streams.is_empty());

        let mut child_events = emitter.take_event_rx("toolu_default");
        let mut saw_output = false;
        let mut saw_known_usage = false;
        for _ in 0..8 {
            let event = recv_child_chat_event(&mut child_events, "tool-first child event").await;
            match event {
                protocol::ChatEvent::StreamDelta(delta) => {
                    saw_output |= delta.text.contains("tool-first complete");
                }
                protocol::ChatEvent::StreamEnd(end) => {
                    saw_known_usage |= end
                        .message
                        .token_usage
                        .as_ref()
                        .and_then(|usage| usage.turn.known_usage())
                        .is_some_and(|usage| usage.total_tokens == 15);
                    break;
                }
                _ => {}
            }
        }
        assert!(saw_output);
        assert!(saw_known_usage);
    }

    #[tokio::test]
    async fn task_started_before_omitted_tool_input_survives_launch_and_reports_usage() {
        let emitter = TestSubAgentEmitter::default();
        let (parent_emitter, _parent_rx) = test_parent_emitter();
        let mut streams = HashMap::new();
        let mut pending_prompts = HashMap::new();

        detect_subagent_task_system_spawns(
            &json!({
                "type": "system", "subtype": "task_started",
                "task_type": "local_agent", "tool_use_id": "toolu_task_first",
                "prompt": "Inspect code"
            }),
            &emitter,
            &parent_emitter,
            &mut streams,
        )
        .await;
        detect_subagent_spawns(
            &json!({
                "type": "assistant", "message": { "content": [{
                    "type": "tool_use", "id": "toolu_task_first", "name": "Agent",
                    "input": { "prompt": "Inspect code" }
                }]}
            }),
            &emitter,
            &parent_emitter,
            &mut streams,
            &mut pending_prompts,
        )
        .await;
        assert_eq!(
            streams.get("toolu_task_first").expect("stream").execution,
            SubAgentExecution::Unknown
        );
        detect_subagent_completions(
            &json!({
                "type": "user", "message": { "content": [{
                    "type": "tool_result", "tool_use_id": "toolu_task_first",
                    "content": "The agent is working in the background. You will be notified automatically when it completes."
                }]}
            }),
            &mut streams,
        )
        .await;
        assert!(streams.contains_key("toolu_task_first"));

        let stream = streams.get_mut("toolu_task_first").expect("stream");
        consume_subagent_event(
            stream,
            &json!({
                "type": "content_block_start", "parent_tool_use_id": "toolu_task_first",
                "index": 0,
                "content_block": {"type": "text", "text": "inspection complete"}
            }),
        );
        consume_subagent_event(
            stream,
            &json!({
                "type": "result", "parent_tool_use_id": "toolu_task_first",
                "result": "inspection complete",
                "usage": {"input_tokens": 20, "output_tokens": 5}
            }),
        );
        finalize_background_subagent_completion(
            &json!({
                "type": "system", "subtype": "task_notification",
                "tool_use_id": "toolu_task_first", "status": "completed"
            }),
            &mut streams,
        );
        assert!(streams.is_empty());

        let mut child_events = emitter.take_event_rx("toolu_task_first");
        let mut saw_output = false;
        let mut saw_known_usage = false;
        for _ in 0..8 {
            let event = recv_child_chat_event(&mut child_events, "task-first child event").await;
            match event {
                protocol::ChatEvent::StreamDelta(delta) => {
                    saw_output |= delta.text.contains("inspection complete");
                }
                protocol::ChatEvent::StreamEnd(end) => {
                    saw_known_usage |= end
                        .message
                        .token_usage
                        .as_ref()
                        .and_then(|usage| usage.turn.known_usage())
                        .is_some_and(|usage| usage.total_tokens == 25);
                    break;
                }
                _ => {}
            }
        }
        assert!(saw_output);
        assert!(saw_known_usage);
    }

    #[tokio::test]
    async fn explicit_foreground_subagent_finishes_on_tool_result() {
        let emitter = TestSubAgentEmitter::default();
        let (parent_emitter, _parent_rx) = test_parent_emitter();
        let mut streams = HashMap::new();
        let mut pending_prompts = HashMap::new();

        detect_subagent_spawns(
            &json!({
                "type": "assistant", "message": { "content": [{
                    "type": "tool_use", "id": "toolu_sync", "name": "Agent",
                    "input": { "prompt": "Inspect", "run_in_background": false }
                }]}
            }),
            &emitter,
            &parent_emitter,
            &mut streams,
            &mut pending_prompts,
        )
        .await;
        detect_subagent_completions(
            &json!({
                "type": "user", "message": { "content": [{
                    "type": "tool_result", "tool_use_id": "toolu_sync", "content": "done"
                }]}
            }),
            &mut streams,
        )
        .await;
        assert!(streams.is_empty());
    }

    #[test]
    fn correlated_frames_distinguish_skills_from_orphaned_children() {
        let streams = HashMap::new();
        let known = HashSet::from(["toolu_child".to_owned()]);
        assert_eq!(
            classify_subagent_correlation(&streams, &known, "toolu_skill"),
            SubAgentCorrelation::Unowned
        );
        assert_eq!(
            classify_subagent_correlation(&streams, &known, "toolu_child"),
            SubAgentCorrelation::Orphaned
        );
    }

    #[test]
    fn correlated_skill_is_nonterminal_while_orphan_is_contextual_diagnostic() {
        let (parent_emitter, mut parent_rx) = test_parent_emitter();
        let mut streams = HashMap::new();
        let known = HashSet::from(["toolu_child".to_owned()]);

        handle_correlated_subagent_event(
            &mut streams,
            &known,
            &parent_emitter,
            "toolu_skill",
            &json!({"type": "assistant", "parent_tool_use_id": "toolu_skill"}),
        );
        assert!(parent_rx.try_recv().is_err());

        handle_correlated_subagent_event(
            &mut streams,
            &known,
            &parent_emitter,
            "toolu_child",
            &json!({"type": "assistant", "parent_tool_use_id": "toolu_child"}),
        );
        let diagnostic = parent_rx.try_recv().expect("orphan diagnostic");
        assert_eq!(
            diagnostic.get("kind").and_then(Value::as_str),
            Some("SubprocessStderr")
        );
        assert!(
            diagnostic
                .get("data")
                .and_then(Value::as_str)
                .is_some_and(|message| message.contains("parent_tool_use_id=toolu_child"))
        );
    }

    #[tokio::test]
    async fn task_started_local_bash_does_not_register_subagent() {
        let emitter = TestSubAgentEmitter::default();
        let (parent_emitter, _parent_rx) = test_parent_emitter();
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
            &parent_emitter,
            &mut streams,
        )
        .await;

        assert!(streams.is_empty());
        assert!(emitter.spawn_records().is_empty());
    }

    #[tokio::test]
    async fn task_started_dedupes_with_later_tool_use_spawn() {
        let emitter = TestSubAgentEmitter::default();
        let (parent_emitter, _parent_rx) = test_parent_emitter();
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
            &parent_emitter,
            &mut streams,
        )
        .await;

        let mut child_events = emitter.take_event_rx("toolu_123");
        let prompt_event =
            recv_child_chat_event(&mut child_events, "initial task prompt event").await;
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
            &parent_emitter,
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
        let message_id = "claude-msg-compact";
        inner.emit_stream_start(message_id, None);

        let stream_start = rx.recv().await.expect("stream start");
        assert_eq!(event_kind(&stream_start), Some("StreamStart"));
        assert_eq!(
            stream_start
                .get("data")
                .and_then(|data| data.get("message_id"))
                .and_then(Value::as_str),
            Some(message_id)
        );

        let mut summary = ClaudeStdoutSummary {
            control_event: Some(ClaudeControlEvent::ConversationCompacted),
            ..ClaudeStdoutSummary::default()
        };

        let emitted = inner
            .emit_terminal_phase_or_placeholder(&mut summary, 0, None, None, None)
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
        assert_eq!(
            stream_end_message(&stream_end)
                .get("message_id")
                .and_then(Value::as_str),
            Some(message_id)
        );
        assert!(
            rx.try_recv().is_err(),
            "compact boundary should not emit an identity error"
        );
    }

    fn make_live_test_inner(
        workspace_root: String,
    ) -> (Arc<ClaudeInner>, mpsc::UnboundedReceiver<Value>) {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        (
            Arc::new(ClaudeInner {
                emitter: Arc::new(TurnEmitter::new_for_agent(
                    event_tx,
                    AgentName(CLAUDE_AGENT_NAME),
                )),
                state: Mutex::new(ClaudeState {
                    workspace_root,
                    ssh_host: None,
                    session_id: None,
                    fork_from_session_id: None,
                    start_session_fresh: false,
                    ephemeral: true,
                    model: None,
                    effort: Some(ClaudeEffort::High),
                    permission_mode: Some(CLAUDE_DEFAULT_PERMISSION_MODE.to_string()),
                    startup_mcp_config_json: None,
                    steering_content: None,
                    agent_identity: None,
                    tool_policy: ToolPolicy::Unrestricted,
                    cumulative_usage: None,
                    cumulative_usage_complete: true,
                    conversation_bytes_total: 0,
                    active_turn: None,
                    restart_process_after_turn: false,
                    subagent_emitter: None,
                    capacity_access: ClaudeCapacityAccess::Unknown,
                    capacity_refresh_in_flight: false,
                    capacity_report_emitted: false,
                    authoritative_capacity_emitted: false,
                }),
                runtime: Mutex::new(None),
                turn_event_gate: Mutex::new(()),
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
                    effort: Some(ClaudeEffort::parse(effort).expect("valid live test effort")),
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

    const RUN_REAL_AI_TESTS_ENV: &str = "TYDE_RUN_REAL_AI_TESTS";
    const RUN_CLAUDE_INTEGRATION_ENV: &str = "TYDE_RUN_CLAUDE_INTEGRATION";

    fn live_claude_tests_enabled() -> bool {
        std::env::var(RUN_REAL_AI_TESTS_ENV).ok().as_deref() == Some("1")
            || std::env::var(RUN_CLAUDE_INTEGRATION_ENV).ok().as_deref() == Some("1")
    }

    fn live_test_workspace_root() -> String {
        std::env::var("TYDE_CLAUDE_TEST_WORKSPACE")
            .unwrap_or_else(|_| env!("CARGO_MANIFEST_DIR").to_string())
    }

    fn skip_live_claude_test() {
        eprintln!(
            "Skipping live Claude integration test; set {RUN_REAL_AI_TESTS_ENV}=1 or {RUN_CLAUDE_INTEGRATION_ENV}=1"
        );
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
    #[ignore = "requires --ignored and TYDE_RUN_REAL_AI_TESTS=1"]
    async fn live_claude_turn_succeeds_at_high_effort() {
        if !live_claude_tests_enabled() {
            skip_live_claude_test();
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
                    summary.usage.is_some() || summary.result_turn_usage.is_some(),
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
    #[ignore = "requires --ignored and TYDE_RUN_REAL_AI_TESTS=1"]
    async fn live_claude_workflow_emits_tool_progress_snapshots() {
        if !live_claude_tests_enabled() {
            skip_live_claude_test();
            return;
        }

        let prompt = "Use the Workflow tool (I explicitly request a workflow) with a trivial \
            script: meta name wfprobe, one phase titled Probe, spawn 2 agents in parallel whose \
            prompts are \"Reply with exactly the word hello and nothing else; do not use any \
            tools.\" Wait for the task notification, then reply with the single word done. \
            ultracode";
        // Don't stop at the end of the turn: the model typically returns
        // while the workflow is still running in the background, and the
        // remaining ToolProgress snapshots arrive BETWEEN turns. That
        // post-turn flow is exactly what this test exists to prove.
        let (inner, mut rx) = make_live_test_inner(live_test_workspace_root());
        inner.clone().start_turn(prompt.to_string(), None).await;

        let mut events = Vec::new();
        let mut snapshots: Vec<WorkflowRunState> = Vec::new();
        loop {
            let event = timeout(Duration::from_secs(300), rx.recv())
                .await
                .unwrap_or_else(|_| {
                    panic!(
                        "timed out waiting for completed workflow snapshot; events: {}",
                        format_live_events(&events)
                    )
                })
                .expect("live Claude event channel closed");
            events.push(event.clone());
            if event_kind(&event) != Some("ToolProgress") {
                continue;
            }
            let Ok(data) = serde_json::from_value::<ToolProgressData>(
                event.get("data").cloned().unwrap_or(Value::Null),
            ) else {
                continue;
            };
            if let ToolProgressUpdate::Workflow(state) = data.update {
                let finished = state.status != WorkflowRunStatus::Running;
                snapshots.push(state);
                if finished {
                    break;
                }
            }
        }

        assert!(
            !snapshots.is_empty(),
            "expected live workflow ToolProgress snapshots; events: {}",
            format_live_events(&events)
        );
        let last = snapshots.last().unwrap();
        assert_eq!(
            last.status,
            WorkflowRunStatus::Completed,
            "final snapshot should be completed; events: {}",
            format_live_events(&events)
        );
        assert!(
            last.agents.len() >= 2
                && last
                    .agents
                    .iter()
                    .all(|agent| agent.state == WorkflowAgentStatus::Done),
            "both workflow agents should finish; final snapshot: {last:?}"
        );
        assert!(last.total_tokens > 0, "usage folded into final snapshot");
    }

    #[tokio::test]
    #[ignore = "requires --ignored and TYDE_RUN_REAL_AI_TESTS=1"]
    async fn live_claude_resume_reports_aggregate_turn_usage() {
        if !live_claude_tests_enabled() {
            skip_live_claude_test();
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

        let request_total = second_summary
            .usage
            .as_ref()
            .and_then(|usage| usage.get("total_tokens"))
            .and_then(Value::as_u64)
            .expect("Expected per-request usage.total_tokens on resumed turn");
        let turn_total = second_summary
            .result_turn_usage
            .as_ref()
            .and_then(|usage| usage.get("total_tokens"))
            .and_then(Value::as_u64)
            .expect("Expected aggregate turn usage.total_tokens from result event");
        eprintln!(
            "LIVE_CLAUDE_TOKEN_USAGE first_request={} first_turn={} second_request={} second_turn={}",
            first_summary
                .usage
                .as_ref()
                .map(Value::to_string)
                .unwrap_or_else(|| "null".to_string()),
            first_summary
                .result_turn_usage
                .as_ref()
                .map(Value::to_string)
                .unwrap_or_else(|| "null".to_string()),
            second_summary
                .usage
                .as_ref()
                .map(Value::to_string)
                .unwrap_or_else(|| "null".to_string()),
            second_summary
                .result_turn_usage
                .as_ref()
                .map(Value::to_string)
                .unwrap_or_else(|| "null".to_string())
        );
        assert!(
            turn_total >= request_total,
            "Expected aggregate turn usage ({turn_total}) to be >= latest request usage ({request_total})"
        );
    }

    #[tokio::test]
    #[ignore = "requires --ignored and TYDE_RUN_REAL_AI_TESTS=1"]
    async fn live_claude_tool_turn_emits_stream_end_before_tool_events() {
        if !live_claude_tests_enabled() {
            skip_live_claude_test();
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
            .cumulative_usage
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
    fn parse_claude_session_replay_sums_unique_model_requests_once() {
        let first = json!({
            "type": "assistant",
            "message": {
                "id": "msg-first",
                "role": "assistant",
                "content": [{ "type": "text", "text": "Working" }],
                "usage": {
                    "input_tokens": 2,
                    "output_tokens": 10,
                    "cache_read_input_tokens": 1_000,
                    "cache_creation_input_tokens": 200,
                    "reasoning_tokens": 3
                }
            }
        });
        let duplicate = json!({
            "type": "assistant",
            "message": {
                "id": "msg-first",
                "role": "assistant",
                "content": [{ "type": "tool_use", "id": "toolu_1", "name": "Read", "input": {"file_path": "a"} }],
                "usage": first["message"]["usage"].clone()
            }
        });
        let second = json!({
            "type": "assistant",
            "message": {
                "id": "msg-second",
                "role": "assistant",
                "content": [{ "type": "text", "text": "Done" }],
                "usage": {
                    "input_tokens": 4,
                    "output_tokens": 20,
                    "cache_read_input_tokens": 2_000,
                    "cache_creation_input_tokens": 300,
                    "reasoning_tokens": 5
                }
            }
        });
        let replay = parse_claude_session_replay(&format!("{first}\n{duplicate}\n{second}\n"));
        let usage = replay.cumulative_usage.expect("reconstructed usage");

        assert_eq!(usage_value_u64(&usage, "input_tokens"), 6);
        assert_eq!(usage_value_u64(&usage, "output_tokens"), 30);
        assert_eq!(usage_value_u64(&usage, "total_tokens"), 36);
        assert_eq!(usage_value_u64(&usage, "cached_prompt_tokens"), 3_000);
        assert_eq!(usage_value_u64(&usage, "cache_creation_input_tokens"), 500);
        assert_eq!(usage_value_u64(&usage, "reasoning_tokens"), 8);
        assert!(replay.cumulative_usage_complete);
    }

    #[test]
    fn parse_claude_session_replay_combines_result_and_assistant_only_invocations() {
        let contents = [
            json!({
                "type": "assistant",
                "message": {
                    "id": "result-backed-request",
                    "role": "assistant",
                    "content": [{ "type": "text", "text": "first" }],
                    "usage": { "input_tokens": 2, "output_tokens": 3 }
                }
            }),
            json!({
                "type": "result",
                "subtype": "success",
                "usage": { "input_tokens": 4, "output_tokens": 6 }
            }),
            json!({
                "type": "assistant",
                "message": {
                    "id": "cancelled-request",
                    "role": "assistant",
                    "content": [{ "type": "text", "text": "partial" }],
                    "usage": {
                        "input_tokens": 7,
                        "output_tokens": 13,
                        "cache_read_input_tokens": 100,
                        "reasoning_tokens": 5
                    }
                }
            }),
            json!({
                "type": "assistant",
                "message": {
                    "id": "cancelled-request",
                    "role": "assistant",
                    "content": [{ "type": "tool_use", "id": "toolu_cancel", "name": "Read", "input": {"file_path": "a"} }],
                    "usage": {
                        "input_tokens": 7,
                        "output_tokens": 13,
                        "cache_read_input_tokens": 100,
                        "reasoning_tokens": 5
                    }
                }
            }),
        ]
        .into_iter()
        .map(|value| value.to_string())
        .collect::<Vec<_>>()
        .join("\n");

        let replay = parse_claude_session_replay(&contents);
        let usage = replay.cumulative_usage.expect("mixed replay usage");
        assert!(replay.cumulative_usage_complete);
        assert_eq!(usage_value_u64(&usage, "input_tokens"), 11);
        assert_eq!(usage_value_u64(&usage, "output_tokens"), 19);
        assert_eq!(usage_value_u64(&usage, "total_tokens"), 30);
        assert_eq!(usage_value_u64(&usage, "cached_prompt_tokens"), 100);
        assert_eq!(usage_value_u64(&usage, "reasoning_tokens"), 5);
    }

    #[test]
    fn replay_user_prompt_boundary_commits_assistant_only_before_later_result() {
        let assistant_a = json!({
            "type": "assistant",
            "message": {
                "id": "request-a",
                "role": "assistant",
                "content": [{ "type": "text", "text": "cancelled A" }],
                "usage": {
                    "input_tokens": 7,
                    "output_tokens": 13,
                    "cache_read_input_tokens": 100
                }
            }
        });
        let assistant_b = json!({
            "type": "assistant",
            "message": {
                "id": "request-b",
                "role": "assistant",
                "content": [{ "type": "text", "text": "B" }],
                "usage": { "input_tokens": 2, "output_tokens": 3 }
            }
        });
        let contents = [
            json!({
                "type": "user",
                "uuid": "user-a",
                "isSidechain": false,
                "promptId": "prompt-a",
                "message": { "role": "user", "content": [{ "type": "text", "text": "A" }] }
            }),
            assistant_a.clone(),
            assistant_a,
            json!({
                "type": "user",
                "uuid": "user-b",
                "isSidechain": false,
                "promptId": "prompt-b",
                "message": { "role": "user", "content": [{ "type": "text", "text": "B" }] }
            }),
            assistant_b.clone(),
            assistant_b,
            json!({
                "type": "result",
                "usage": { "input_tokens": 4, "output_tokens": 6 }
            }),
        ]
        .into_iter()
        .map(|value| value.to_string())
        .collect::<Vec<_>>()
        .join("\n");

        let replay = parse_claude_session_replay(&contents);
        assert!(replay.cumulative_usage_complete);
        let usage = replay.cumulative_usage.expect("boundary usage");
        assert_eq!(usage_value_u64(&usage, "input_tokens"), 11);
        assert_eq!(usage_value_u64(&usage, "output_tokens"), 19);
        assert_eq!(usage_value_u64(&usage, "total_tokens"), 30);
        assert_eq!(usage_value_u64(&usage, "cached_prompt_tokens"), 100);
    }

    #[tokio::test]
    async fn replay_user_prompt_boundary_preserves_prior_missing_id_ambiguity() {
        let missing_a = json!({
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [{ "type": "text", "text": "cancelled A" }],
                "usage": { "input_tokens": 7, "output_tokens": 13 }
            }
        });
        let assistant_b = json!({
            "type": "assistant",
            "message": {
                "id": "request-b",
                "role": "assistant",
                "content": [{ "type": "text", "text": "B" }],
                "usage": { "input_tokens": 2, "output_tokens": 3 }
            }
        });
        let contents = [
            json!({
                "type": "user",
                "uuid": "user-a",
                "isSidechain": false,
                "promptId": "prompt-a",
                "message": { "role": "user", "content": [{ "type": "text", "text": "A" }] }
            }),
            missing_a.clone(),
            missing_a,
            json!({
                "type": "user",
                "uuid": "user-b",
                "isSidechain": false,
                "promptId": "prompt-b",
                "message": { "role": "user", "content": [{ "type": "text", "text": "B" }] }
            }),
            assistant_b.clone(),
            assistant_b,
            json!({
                "type": "result",
                "usage": { "input_tokens": 4, "output_tokens": 6 }
            }),
        ]
        .into_iter()
        .map(|value| value.to_string())
        .collect::<Vec<_>>()
        .join("\n");

        let replay = parse_claude_session_replay(&contents);
        assert!(!replay.cumulative_usage_complete);
        assert_eq!(
            replay
                .cumulative_usage
                .as_ref()
                .map(|usage| usage_value_u64(usage, "total_tokens")),
            Some(10)
        );

        let (inner, mut rx) = make_test_inner();
        let message_id = "claude-msg-replay-boundary";
        inner.emit_stream_start(message_id, None);
        let stream_start = rx.recv().await.expect("replay boundary StreamStart");
        assert_eq!(event_kind(&stream_start), Some("StreamStart"));
        assert_eq!(
            stream_start
                .get("data")
                .and_then(|data| data.get("message_id"))
                .and_then(Value::as_str),
            Some(message_id)
        );
        {
            let mut state = inner.state.lock().await;
            state.cumulative_usage = replay.cumulative_usage;
            state.cumulative_usage_complete = replay.cumulative_usage_complete;
        }
        let usage = inner
            .normalize_usage_for_turn(Some(json!({
                "input_tokens": 3,
                "output_tokens": 2,
                "total_tokens": 5
            })))
            .await
            .expect("known current turn");
        inner.emit_placeholder_stream_end(None, Some(usage), None);
        let raw = rx
            .recv()
            .await
            .expect("identified replay boundary StreamEnd");
        assert_eq!(event_kind(&raw), Some("StreamEnd"));
        let event: ChatEvent = serde_json::from_value(raw).expect("typed ambiguous usage");
        let ChatEvent::StreamEnd(end) = event else {
            panic!("expected StreamEnd");
        };
        assert_eq!(
            end.message.message_id.as_ref().map(|id| id.0.as_str()),
            Some(message_id)
        );
        assert!(matches!(
            end.message.token_usage.expect("usage").cumulative,
            protocol::TokenUsageScope::Unavailable {
                reason: protocol::TokenUsageUnavailableReason::ProviderScopeAmbiguous
            }
        ));
        assert!(
            rx.try_recv().is_err(),
            "replay boundary should not emit an identity error or cancellation"
        );
    }

    #[tokio::test]
    async fn parse_claude_session_replay_marks_missing_usage_identity_unreportable() {
        let covered = [
            json!({
                "type": "assistant",
                "message": {
                    "role": "assistant",
                    "content": [{ "type": "text", "text": "covered" }],
                    "usage": { "input_tokens": 2, "output_tokens": 3 }
                }
            }),
            json!({
                "type": "result",
                "usage": { "input_tokens": 4, "output_tokens": 6 }
            }),
        ]
        .into_iter()
        .map(|value| value.to_string())
        .collect::<Vec<_>>()
        .join("\n");
        let covered = parse_claude_session_replay(&covered);
        assert!(covered.cumulative_usage_complete);
        assert_eq!(
            covered
                .cumulative_usage
                .as_ref()
                .map(|usage| usage_value_u64(usage, "total_tokens")),
            Some(10)
        );

        let contents = [
            json!({
                "type": "result",
                "usage": { "input_tokens": 4, "output_tokens": 6 }
            }),
            json!({
                "type": "assistant",
                "message": {
                    "role": "assistant",
                    "content": [{ "type": "text", "text": "partial" }],
                    "usage": { "input_tokens": 7, "output_tokens": 13 }
                }
            }),
        ]
        .into_iter()
        .map(|value| value.to_string())
        .collect::<Vec<_>>()
        .join("\n");

        let replay = parse_claude_session_replay(&contents);
        assert!(!replay.cumulative_usage_complete);
        assert_eq!(
            replay
                .cumulative_usage
                .as_ref()
                .map(|usage| usage_value_u64(usage, "total_tokens")),
            Some(10)
        );

        let (inner, mut rx) = make_test_inner();
        let message_id = "claude-msg-replay-missing-usage-id";
        inner.emit_stream_start(message_id, None);
        let stream_start = rx.recv().await.expect("replay usage StreamStart");
        assert_eq!(event_kind(&stream_start), Some("StreamStart"));
        assert_eq!(
            stream_start
                .get("data")
                .and_then(|data| data.get("message_id"))
                .and_then(Value::as_str),
            Some(message_id)
        );
        {
            let mut state = inner.state.lock().await;
            state.cumulative_usage = replay.cumulative_usage;
            state.cumulative_usage_complete = replay.cumulative_usage_complete;
        }
        let usage = inner
            .normalize_usage_for_turn(Some(json!({
                "input_tokens": 3,
                "output_tokens": 2,
                "total_tokens": 5
            })))
            .await
            .expect("known current turn");
        assert_eq!(usage_value_u64(&usage.turn, "total_tokens"), 5);
        assert_eq!(usage.cumulative, None);
        inner.emit_placeholder_stream_end(None, Some(usage), None);
        let raw = rx.recv().await.expect("identified replay usage StreamEnd");
        assert_eq!(event_kind(&raw), Some("StreamEnd"));
        let event: ChatEvent = serde_json::from_value(raw).expect("typed replay usage terminal");
        let ChatEvent::StreamEnd(end) = event else {
            panic!("expected StreamEnd");
        };
        assert_eq!(
            end.message.message_id.as_ref().map(|id| id.0.as_str()),
            Some(message_id)
        );
        let token_usage = end.message.token_usage.expect("typed token usage");
        assert_eq!(
            token_usage
                .turn
                .known_usage()
                .map(|usage| usage.total_tokens),
            Some(5)
        );
        assert!(matches!(
            token_usage.cumulative,
            protocol::TokenUsageScope::Unavailable {
                reason: protocol::TokenUsageUnavailableReason::ProviderScopeAmbiguous
            }
        ));
        assert!(
            rx.try_recv().is_err(),
            "replay usage terminal should not emit an identity error or cancellation"
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
    fn claude_tool_request_type_maps_ask_user_question() {
        let request = claude_tool_request_type(
            "AskUserQuestion",
            &json!({
                "questions": [{
                    "id": "language",
                    "question": "Which language?",
                    "header": "Language",
                    "options": [
                        { "label": "Rust", "description": "Systems lang" },
                        { "label": "Python", "description": "Scripting lang" }
                    ],
                    "multiSelect": false
                }]
            }),
        );

        let parsed: protocol::ToolRequestType =
            serde_json::from_value(request).expect("typed AskUserQuestion request");
        let protocol::ToolRequestType::AskUserQuestion { questions } = parsed else {
            panic!("expected AskUserQuestion request");
        };
        assert_eq!(questions.len(), 1);
        let question = &questions[0];
        assert_eq!(question.id.as_deref(), Some("language"));
        assert_eq!(question.question, "Which language?");
        assert_eq!(question.header.as_deref(), Some("Language"));
        assert!(!question.multi_select);
        assert_eq!(question.options.len(), 2);
        assert_eq!(question.options[0].label, "Rust");
        assert_eq!(
            question.options[0].description.as_deref(),
            Some("Systems lang")
        );
    }

    #[test]
    fn claude_tool_request_type_maps_top_level_prompt_ask_user_question() {
        let request = claude_tool_request_type(
            "AskUserQuestion",
            &json!({
                "prompt": "Continue?"
            }),
        );

        let parsed: protocol::ToolRequestType =
            serde_json::from_value(request).expect("typed AskUserQuestion request");
        let protocol::ToolRequestType::AskUserQuestion { questions } = parsed else {
            panic!("expected AskUserQuestion request");
        };
        assert_eq!(questions.len(), 1);
        assert_eq!(questions[0].question, "Continue?");
        assert_eq!(questions[0].header, None);
        assert!(questions[0].options.is_empty());
        assert!(!questions[0].multi_select);
    }

    #[test]
    fn claude_tool_request_type_maps_exit_plan_mode() {
        let request = claude_tool_request_type(
            "ExitPlanMode",
            &json!({
                "plan": "# Plan\n\nDo the work.",
                "planFilePath": "/repo/.claude/plans/test.md",
            }),
        );

        let parsed: protocol::ToolRequestType =
            serde_json::from_value(request).expect("typed ExitPlanMode request");
        let protocol::ToolRequestType::ExitPlanMode { plan, plan_path } = parsed else {
            panic!("expected ExitPlanMode tool type");
        };
        assert_eq!(plan.as_deref(), Some("# Plan\n\nDo the work."));
        assert_eq!(plan_path.as_deref(), Some("/repo/.claude/plans/test.md"));
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

    #[tokio::test]
    async fn result_usage_accumulates_per_turn_without_overwriting_session_totals() {
        let (inner, _rx) = make_test_inner();
        let first = json!({
            "input_tokens": 5,
            "output_tokens": 10,
            "total_tokens": 15,
            "cached_prompt_tokens": 20_000,
            "cache_creation_input_tokens": 500,
            "reasoning_tokens": 2,
            "context_window": 200_000
        });
        let second = json!({
            "input_tokens": 3,
            "output_tokens": 7,
            "total_tokens": 10,
            "cached_prompt_tokens": 4_000,
            "cache_creation_input_tokens": 100,
            "reasoning_tokens": 5,
            "context_window": 200_000
        });

        let first_usage = inner
            .normalize_usage_for_turn(Some(first.clone()))
            .await
            .expect("first turn usage");
        assert_eq!(first_usage.turn, first);
        assert_eq!(first_usage.cumulative, Some(first));

        let second_usage = inner
            .normalize_usage_for_turn(Some(second.clone()))
            .await
            .expect("second turn usage");
        assert_eq!(second_usage.turn, second);
        assert_eq!(
            second_usage.cumulative,
            Some(json!({
                "input_tokens": 8,
                "output_tokens": 17,
                "total_tokens": 25,
                "cached_prompt_tokens": 24_000,
                "cache_creation_input_tokens": 600,
                "reasoning_tokens": 7,
                "context_window": 200_000
            }))
        );
        let first_cumulative = first_usage.cumulative.as_ref().expect("first cumulative");
        let second_cumulative = second_usage.cumulative.as_ref().expect("second cumulative");
        assert!(
            usage_value_u64(second_cumulative, "total_tokens")
                >= usage_value_u64(first_cumulative, "total_tokens")
        );
        assert_eq!(
            inner.state.lock().await.cumulative_usage,
            Some(second_cumulative.clone())
        );
    }

    #[test]
    fn derive_turn_token_usage_deltas_cumulative_snapshots_and_reset() {
        let first = json!({
            "input_tokens": 400,
            "output_tokens": 100,
            "total_tokens": 500,
            "cached_prompt_tokens": 250,
            "cache_creation_input_tokens": 20,
            "reasoning_tokens": 15,
            "context_window": 200_000
        });
        let second = json!({
            "input_tokens": 900,
            "output_tokens": 180,
            "total_tokens": 1_080,
            "cached_prompt_tokens": 650,
            "cache_creation_input_tokens": 35,
            "reasoning_tokens": 45,
            "context_window": 200_000
        });
        let reset = json!({
            "input_tokens": 120,
            "output_tokens": 25,
            "total_tokens": 145,
            "cached_prompt_tokens": 80,
            "cache_creation_input_tokens": 5,
            "reasoning_tokens": 7,
            "context_window": 200_000
        });

        let first_turn = derive_turn_token_usage(&first, None).expect("first turn usage");
        assert_eq!(first_turn, first);

        let second_turn =
            derive_turn_token_usage(&second, Some(&first)).expect("second turn usage");
        assert_eq!(
            second_turn,
            json!({
                "input_tokens": 500,
                "output_tokens": 80,
                "total_tokens": 580,
                "cached_prompt_tokens": 400,
                "cache_creation_input_tokens": 15,
                "reasoning_tokens": 30,
                "context_window": 200_000
            })
        );

        for key in [
            "input_tokens",
            "output_tokens",
            "total_tokens",
            "cached_prompt_tokens",
            "cache_creation_input_tokens",
            "reasoning_tokens",
        ] {
            assert!(
                second_turn.get(key).and_then(Value::as_u64).is_some(),
                "{key} should be a non-negative token count"
            );
        }

        let reset_turn = derive_turn_token_usage(&reset, Some(&second)).expect("reset turn usage");
        assert_eq!(reset_turn, reset);
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
    fn parse_claude_effort_preserves_native_levels_and_unset() {
        for (input, expected) in [
            ("low", ClaudeEffort::Low),
            (" Medium ", ClaudeEffort::Medium),
            ("HIGH", ClaudeEffort::High),
            ("xhigh", ClaudeEffort::XHigh),
            ("max", ClaudeEffort::Max),
        ] {
            assert_eq!(
                parse_claude_effort_setting(&Value::String(input.to_string())),
                Ok(Some(expected))
            );
        }
        assert_eq!(parse_claude_effort_setting(&Value::Null), Ok(None));
        assert_eq!(
            parse_claude_effort_setting(&Value::String("  ".to_string())),
            Ok(None)
        );
    }

    #[test]
    fn parse_claude_effort_rejects_aliases_and_unknown_values() {
        for value in ["extra_high", "extra-high", "minimal", "none", "ultra"] {
            let error = parse_claude_effort_setting(&Value::String(value.to_string()))
                .expect_err("non-native Claude effort should fail");
            assert!(error.contains("Claude effort"));
            assert!(error.contains(value));
            for valid in ["low", "medium", "high", "xhigh", "max"] {
                assert!(
                    error.contains(valid),
                    "Claude effort error should list valid value {valid}: {error}"
                );
            }
        }
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
        // 2. result event with aggregate turn usage
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
        // result_turn_usage, NOT overwrite usage.
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

        // usage should still be the per-API-call value (not the turn aggregate)
        let usage = summary.usage.as_ref().expect("usage should be set");
        assert_eq!(usage.get("input_tokens").and_then(Value::as_u64), Some(1));
        assert_eq!(
            usage.get("cached_prompt_tokens").and_then(Value::as_u64),
            Some(20_000)
        );

        // result_turn_usage should hold the turn aggregate from result
        let cum = summary
            .result_turn_usage
            .as_ref()
            .expect("result_turn_usage should be set");
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
    fn estimate_context_breakdown_treats_fable_as_1m_window() {
        // Fable defaults to a 1M context window even without an explicit [1m]
        // suffix, so the estimated fallback must not collapse it to 200k.
        let usage = json!({
            "input_tokens": 20,
            "output_tokens": 10,
            "total_tokens": 30,
            "cached_prompt_tokens": 0,
            "cache_creation_input_tokens": 0,
            "reasoning_tokens": 0
        });
        for model in ["fable", "claude-fable-5", "Claude Fable 5"] {
            let bd = estimate_context_breakdown(Some(&usage), 0, 0, 0, None, Some(model));
            assert_eq!(
                bd.get("context_window").and_then(Value::as_u64),
                Some(CLAUDE_ESTIMATED_CONTEXT_WINDOW_1M),
                "{model} should estimate a 1M context window"
            );
        }
    }

    #[test]
    fn phase_usage_emits_per_api_call_request_usage() {
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

        let usage = phase_usage_for_emission(&mut summary).expect("request usage");
        assert_eq!(
            usage.get("total_tokens").and_then(Value::as_u64),
            Some(150_500)
        );
        assert!(summary.usage.is_none());
    }

    #[test]
    fn todo_write_emits_task_update() {
        use protocol::TaskStatus;
        let arguments = json!({
            "todos": [
                {"content": "Fix the bug", "status": "completed", "activeForm": "Fixing the bug"},
                {"content": "Run tests", "status": "in_progress", "activeForm": "Running tests"},
                {"content": "Deploy", "status": "pending", "activeForm": "Deploying"},
            ]
        });
        let tasks = claude_task_update_from_todo_write(&arguments)
            .expect("should produce a TaskList payload");
        assert_eq!(tasks.title, "");
        assert_eq!(tasks.tasks.len(), 3);
        assert_eq!(tasks.tasks[0].description, "Fix the bug");
        assert!(matches!(tasks.tasks[0].status, TaskStatus::Completed));
        assert_eq!(tasks.tasks[1].description, "Running tests");
        assert!(matches!(tasks.tasks[1].status, TaskStatus::InProgress));
        assert_eq!(tasks.tasks[2].description, "Deploy");
        assert!(matches!(tasks.tasks[2].status, TaskStatus::Pending));
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
        spawn_claude_subagent_event_bridge(raw_event_rx, relay_event_tx.clone());
        let mut streams = HashMap::new();
        streams.insert(
            "toolu_spawn".to_string(),
            SubAgentStream {
                summary: ClaudeStdoutSummary::default(),
                segment: SegmentState::default(),
                message_id: "subagent-toolu_spawn".to_string(),
                has_explicit_task_prompt: false,
                inner: Arc::new(ClaudeInner {
                    emitter: Arc::new(TurnEmitter::new_for_agent(
                        raw_event_tx,
                        AgentName(CLAUDE_AGENT_NAME),
                    )),
                    state: Mutex::new(ClaudeState::default()),
                    runtime: Mutex::new(None),
                    turn_event_gate: Mutex::new(()),
                }),
                parent_tool_use_id: "toolu_spawn".to_string(),
                agent_id: protocol::AgentId("test-subagent".to_string()),
                agent_name: "Agent".to_string(),
                parent_emitter: test_parent_emitter().0,
                last_progress_emit: std::time::Instant::now(),
                execution: SubAgentExecution::Foreground,
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

        let request = events
            .iter()
            .find(|ev| event_kind(ev) == Some("ToolRequest"))
            .expect("ToolRequest should be present");
        assert_eq!(
            request
                .pointer("/data/tool_type/kind")
                .and_then(Value::as_str),
            Some("AskUserQuestion")
        );
        let parsed_request: protocol::ToolRequest =
            serde_json::from_value(request["data"].clone()).expect("typed ToolRequest");
        let protocol::ToolRequestType::AskUserQuestion { questions } = parsed_request.tool_type
        else {
            panic!("expected AskUserQuestion tool type");
        };
        assert_eq!(questions.len(), 1);
        assert_eq!(questions[0].question, "Which language?");
        assert_eq!(questions[0].header.as_deref(), Some("Language"));
        assert_eq!(questions[0].options.len(), 2);
        assert_eq!(questions[0].options[0].label, "Rust");
        assert_eq!(
            questions[0].options[0].description.as_deref(),
            Some("Systems lang")
        );
        assert!(!questions[0].multi_select);

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
        let (events_tx, mut events_rx) = mpsc::unbounded_channel::<ChatEvent>();
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
