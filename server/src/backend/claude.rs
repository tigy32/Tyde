use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
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
    CapacityUnavailableReason, CapacityWindow, ClaudeLimitType, ContextBreakdown,
    ExitPlanModeDecision, ImageData, MessageTokenUsage, ModelInfo, ReasoningData,
    SendMessageToolResponse, SessionId, TokenUsage, TokenUsageScope, TokenUsageUnavailableReason,
    ToolExecutionMode, ToolExecutionOutcome, ToolExecutionResult, ToolPolicy, ToolProgressData,
    ToolProgressUpdate, ToolRequestType, ToolUseData, ValueProvenance, WorkflowAgentState,
    WorkflowAgentStatus, WorkflowRunState, WorkflowRunStatus,
};

use crate::agent::customization::SkillSelection;
use crate::backend::claude_skills::{
    CLAUDE_PLUGIN_DIR_FLAG, ClaudeSkillPlugin, InitFrameVerdict, PreparedSkill,
    degraded_default_notice, help_text_supports_plugin_dir, native_skill_overlay,
    unsupported_plugin_dir_notice, verify_init_frame, verify_plugin_inventory,
};
use crate::backend::turn_emitter::{
    AgentName, AssistantMessagePayload, ResponseHandle, RetryAttemptPayload, StreamEndPayload,
    TurnEmitter,
};
use crate::backend::{
    AgentIdentity, READ_ONLY_ACCESS_MODE_INSTRUCTIONS, SessionCommand, StartupMcpServer,
    StartupMcpTransport, normalize_mcp_call_tool_result,
};
use crate::process_env;
use crate::sub_agent::SubAgentEmitter;
use crate::subprocess::ImageAttachment;

/// Per-sub-agent stream state, tracking its own summary and segment.
struct SubAgentStream {
    summary: ClaudeStdoutSummary,
    segment: SegmentState,
    message_id: String,
    /// A local ClaudeInner that routes events to the sub-agent's channel.
    inner: Arc<ClaudeInner>,
    /// The parent's Task tool_use id — the `tool_call_id` for live
    /// `ToolProgress` updates on the parent's Task tool card.
    parent_tool_use_id: String,
    parent_tool_name: String,
    /// Id of the spawned sub-agent (from `SubAgentHandle`), included in
    /// progress updates so the frontend can link to the sub-agent view.
    agent_id: protocol::AgentId,
    agent_name: String,
    name_update_tx: Option<mpsc::UnboundedSender<String>>,
    /// Emitter of the PARENT agent, used for the progress updates above.
    parent_emitter: Arc<TurnEmitter>,
    last_progress_emit: std::time::Instant,
    /// How the CLI classified this sub-agent's execution lifecycle.
    /// Background agents keep streaming their own output *after* the parent
    /// receives the synthetic "launched" tool_result, so their stream must
    /// be finalized on the `task_notification` completion frame rather than
    /// torn down early when that placeholder tool_result arrives.
    execution: SubAgentExecution,
    /// Lifetime telemetry. `ClaudeStdoutSummary::tool_calls` is phase-local
    /// and is cleared after every tool result, so it cannot back the parent
    /// card's final count.
    seen_tool_call_ids: HashSet<String>,
    last_tool_name: Option<String>,
    reported_total_tokens: Option<u64>,
    execution_failed: bool,
    pending_terminal: Option<(String, Option<String>)>,
    pending_parent_progress: VecDeque<ToolProgressData>,
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
const CLAUDE_FREE_TEXT_SENTINEL: &str = "TYDE_FREE_TEXT";
const CLAUDE_FREE_TEXT_OTHER: &str = "Other";

const CLAUDE_ESTIMATED_CONTEXT_WINDOW_DEFAULT: u64 = 200_000;
const CLAUDE_ESTIMATED_CONTEXT_WINDOW_1M: u64 = 1_000_000;
const CLAUDE_ESTIMATED_BYTES_PER_TOKEN: u64 = 4;
const CLAUDE_MIN_SYSTEM_PROMPT_BYTES: u64 = 1_024;
const CLAUDE_DEFAULT_PERMISSION_MODE: &str = "bypassPermissions";
/// How long the stdout reader waits for a finished turn's finalizer to hand
/// off before adopting a CLI wake turn. Bounded because the reader is blocked
/// meanwhile; the frames it is waiting to route simply queue behind it.
const CLAUDE_WAKE_QUIESCE_WAIT: Duration = Duration::from_secs(5);
// Claude plan mode blocks build/test Bash; ReadOnly is advisory in Tyde.
const CLAUDE_INITIALIZE_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a session will hold its output waiting for the CLI to report which
/// skills it loaded. Matches the provider-process handshake timeout.
const CLAUDE_SKILL_VERIFICATION_TIMEOUT: Duration = CLAUDE_INITIALIZE_TIMEOUT;
/// How much output a session will hold while waiting to learn whether it has
/// its skills. Generous enough that a normal first response never trips it, and
/// bounded so a CLI that never reports cannot grow the buffer without limit.
const CLAUDE_HELD_BACK_FRAME_LIMIT: usize = 512;
const CLAUDE_HELD_BACK_BYTE_LIMIT: usize = 4 * 1024 * 1024;

const CLAUDE_CONTROL_RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);
const CLAUDE_INTERRUPT_QUIESCE_TIMEOUT: Duration = Duration::from_secs(18);
const CLAUDE_COMPACTION_TIMEOUT: Duration = Duration::from_secs(300);
const TYDE_CLAUDE_BIN_ENV: &str = "TYDE_CLAUDE_BIN";

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
        match ClaudeInner::send_message(
            Arc::clone(&self.inner),
            payload.message,
            protocol_images_to_attachments(payload.images),
            payload.tool_response,
        )
        .await?
        {
            ClaudeSendAdmission::Handled => Ok(()),
            // Busy sends flow through `send_message_with_outcome`, which
            // hands the message back for requeueing. This legacy path has no
            // way to return it, so a busy result here is an invariant breach
            // and must be visible rather than silently dropped.
            ClaudeSendAdmission::Busy => {
                self.inner
                    .emit_error("Claude is busy with another turn; the message was not delivered.");
                Err("Claude backend was busy on a path that cannot requeue".to_string())
            }
        }
    }

    /// Like `send_message_payload`, but reports a busy backend by handing the
    /// payload back to the caller instead of failing.
    async fn send_message_with_outcome(
        &self,
        payload: protocol::SendMessagePayload,
    ) -> Result<ClaudeSendAdmission, String> {
        ClaudeInner::send_message(
            Arc::clone(&self.inner),
            payload.message,
            protocol_images_to_attachments(payload.images),
            payload.tool_response,
        )
        .await
    }

    fn compaction_capability(&self) -> BackendCompactionCapability {
        self.inner
            .state
            .try_lock()
            .map(|state| state.compaction_capability.clone())
            .unwrap_or_else(|_| {
                BackendCompactionCapability::unknown(
                    BackendCompactionUnknownReason::CapabilityProbeFailed(
                        "Claude session state is busy".to_string(),
                    ),
                    None,
                    BackendCompactionCapabilityEvidence::None,
                )
            })
    }

    async fn begin_compaction(&self, request: BackendCompactionRequest) -> BackendCompactionStart {
        ClaudeInner::begin_compaction(Arc::clone(&self.inner), request).await
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
    skills: ClaudeSkillExposure,
}

struct ClaudeForkConfig<'a> {
    from_session_id: &'a str,
    ssh_host: Option<String>,
    startup_mcp_servers: &'a [StartupMcpServer],
    steering_content: Option<&'a str>,
    agent_identity: Option<&'a AgentIdentity>,
    tool_policy: ToolPolicy,
    access_mode: BackendAccessMode,
    skills: ClaudeSkillExposure,
}

/// What the session ended up doing about skills, carried from the one place
/// that materializes into the session state that outlives every respawn.
#[derive(Debug, Default)]
struct ClaudeSkillExposure {
    plugin: Option<Arc<ClaudeSkillPlugin>>,
    /// `tyde-skills:<name>` for every materialized skill. The `init` frame must
    /// report all of these before the session's first prompt.
    expected: Vec<String>,
    /// User-visible notice for a Default session that started degraded. Never a
    /// silent omission.
    degraded_notice: Option<String>,
}

/// Decide how a Claude session exposes its selected skills, and materialize the
/// session plugin when it is a local session.
///
/// **Nothing about a skill stops a session from starting.** A skill that cannot
/// be exposed — a refusal, an unreadable body, a name collision, a CLI too old
/// for `--plugin-dir`, an SSH target that cannot see this machine's disk — costs
/// the session that one capability. Refusing to start costs it that capability
/// *and* every other skill, the agent, and the conversation, which is a strictly
/// worse answer to the same problem. This holds for an explicit selection too:
/// a custom agent short one skill is still the agent the user asked for.
///
/// **What is never allowed is silence.** Every omission produces a user-visible
/// notice naming the skill and the reason, and the overlay is built solely from
/// the skills that actually materialized, so the model is never told about a
/// skill this session does not have.
///
/// There is deliberately no fallback to pasting bodies into a local prompt: that
/// is the behaviour this work removes, and doing it silently on a capability
/// miss would make the regression invisible.
async fn claude_prepare_skills(
    config: &BackendSpawnConfig,
    ssh_host: Option<&str>,
    workspace_root: &str,
) -> (ClaudeSkillExposure, ClaudeSkillSteering) {
    let selected = &config.resolved_spawn_config.skills;
    let selection = config.resolved_spawn_config.skill_selection;
    if selected.is_empty() {
        return (ClaudeSkillExposure::default(), ClaudeSkillSteering::None);
    }

    // Every exit below that gives up on skills routes through this, so no path
    // can drop one without telling the user which and why.
    let without_skills = |notice: String| {
        tracing::warn!("{notice}");
        (
            ClaudeSkillExposure {
                plugin: None,
                expected: Vec::new(),
                degraded_notice: Some(notice),
            },
            ClaudeSkillSteering::None,
        )
    };

    // Keyed on the host actually handed to the low-level spawn, not on the shape
    // of the workspace roots: a root list that merely mentions `ssh://` does not
    // make this process remote, and must not change how a local session works.
    //
    // Remote native skills are not supported. A plugin root materialized on this
    // machine is invisible to a CLI running somewhere else, and Tyde has no
    // remote materialization path. Earlier revisions inlined bodies here
    // instead; that branch was unreachable from any production entry point, so
    // it documented a fallback that did not exist. The session runs remotely
    // without them, and says so.
    if let Some(host) = ssh_host {
        return without_skills(format!(
            "Tyde started this Claude session without its {} selected skill(s): Tyde \
             materializes them into a directory on the machine it runs on, which the CLI on \
             '{host}' cannot read. The session works normally otherwise, and any skills \
             installed on '{host}' are still available.",
            selected.len()
        ));
    }

    if !claude_supports_plugin_dir().await {
        return without_skills(unsupported_plugin_dir_notice());
    }

    let outcome = match ClaudeSkillPlugin::prepare(None, selected) {
        Ok(outcome) => outcome,
        Err(err) => {
            return without_skills(format!(
                "Tyde started this Claude session without its {} selected skill(s): {err}",
                selected.len()
            ));
        }
    };
    for refusal in &outcome.refusals {
        tracing::warn!("Claude skill materialization: {}", refusal.describe());
    }

    let degraded_notice =
        (!outcome.refusals.is_empty()).then(|| degraded_default_notice(&outcome.refusals));

    let Some(plugin) = outcome.plugin else {
        // Every selected skill was refused. The session still starts, but never
        // silently: the notice above names each one.
        return (
            ClaudeSkillExposure {
                plugin: None,
                expected: Vec::new(),
                degraded_notice,
            },
            ClaudeSkillSteering::None,
        );
    };

    // Zero-provider, pre-start, machine-readable: find out whether the CLI
    // actually loaded this root before the session process exists. Catches every
    // *global* failure — a rejected flag, an unreadable manifest, a disabled
    // plugin — early enough to tell the user in one notice instead of letting
    // each skill turn up missing mid-turn.
    if let Err(err) = claude_verify_plugin_loaded(plugin.root(), workspace_root).await {
        let mut notice = format!(
            "Tyde started this Claude session without its {} selected skill(s): {err}.",
            selected.len()
        );
        if let Some(refused) = degraded_notice.as_deref() {
            notice.push_str("\n\n");
            notice.push_str(refused);
        }
        return without_skills(notice);
    }

    let expected = plugin.exposed().to_vec();
    tracing::debug!(
        "Claude session exposes {} skill(s) via {}: {}",
        expected.len(),
        plugin.root().display(),
        expected.join(", ")
    );
    (
        ClaudeSkillExposure {
            plugin: Some(Arc::new(plugin)),
            expected,
            degraded_notice,
        },
        ClaudeSkillSteering::Native(selection, outcome.prepared),
    )
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
        Self::spawn_with_skills(
            workspace_roots,
            ssh_host,
            startup_mcp_servers,
            steering_content,
            agent_identity,
            tool_policy,
            access_mode,
            ClaudeSkillExposure::default(),
        )
        .await
    }

    /// [`spawn`](Self::spawn), taking ownership of the session's inline skill
    /// plugin so the root outlives every process the session starts.
    #[allow(clippy::too_many_arguments)]
    async fn spawn_with_skills(
        workspace_roots: &[String],
        ssh_host: Option<String>,
        startup_mcp_servers: &[StartupMcpServer],
        steering_content: Option<&str>,
        agent_identity: Option<&AgentIdentity>,
        tool_policy: ToolPolicy,
        access_mode: BackendAccessMode,
        skills: ClaudeSkillExposure,
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
                skills,
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
                skills: ClaudeSkillExposure::default(),
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
                skills: fork_config.skills,
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
        let ClaudeSkillExposure {
            plugin: skill_plugin,
            expected: expected_skills,
            degraded_notice,
        } = mode.skills;

        let inner = Arc::new(ClaudeInner {
            emitter: Arc::new(TurnEmitter::new_for_agent(
                event_tx,
                AgentName(CLAUDE_AGENT_NAME),
            )),
            active_response: StdMutex::new(None),
            state: Mutex::new(ClaudeState {
                workspace_root,
                ssh_host: resolved_ssh_host,
                session_id: None,
                fork_from_session_id: mode.fork_from_session_id,
                start_session_fresh: false,
                resume_bootstrap_required: true,
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
                skill_plugin,
                // Populated by `arm_skill_verification` below.
                expected_skills: Vec::new(),
                skill_verification_generation: 0,
                skill_watchdog: None,
                cumulative_usage: None,
                cumulative_usage_complete: true,
                conversation_bytes_total: 0,
                active_turn: None,
                compaction_capability: BackendCompactionCapability::unknown(
                    BackendCompactionUnknownReason::ProcessNotInitialized,
                    None,
                    BackendCompactionCapabilityEvidence::None,
                ),
                compact_command_advertised: None,
                installed_provider_version: None,
                provider_version: None,
                process_generation: 0,
                resume_bootstrap: None,
                resume_empty_result_generation: None,
                pending_compaction: None,
                closing: false,
                restart_process_after_turn: false,
                subagent_emitter: None,
                capacity_access: ClaudeCapacityAccess::Unknown,
                capacity_refresh_in_flight: false,
                capacity_report_emitted: false,
                authoritative_capacity_emitted: false,
            }),
            runtime: Mutex::new(None),
            turn_event_gate: Mutex::new(()),
            task_tracker: StdMutex::new(ClaudeTaskTracker::default()),
            background_tasks: StdMutex::new(BackgroundTaskRegistry::active()),
            native_subagent_tasks: StdMutex::new(HashSet::new()),
            skill_readiness: watch::channel(ClaudeSkillReadiness::NotRequired).0,
            skill_verification_abandoned: std::sync::atomic::AtomicBool::new(false),
            pending_cli_wake: std::sync::atomic::AtomicBool::new(false),
            background_work_active: std::sync::atomic::AtomicBool::new(false),
            typing_active: std::sync::atomic::AtomicBool::new(false),
        });

        // Armed through the same call every other caller uses, immediately after
        // construction and long before any prompt can be written. Setting the
        // initial channel value here instead would have been a second arming
        // path that tests could not exercise.
        inner.arm_skill_verification(expected_skills).await;

        // A Default session that dropped a skill says so. The channel is
        // unbounded and the receiver is handed back below, so this arrives even
        // though nothing is listening yet.
        if let Some(notice) = degraded_notice.as_deref() {
            inner.emitter.subprocess_stderr(notice);
        }

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

    async fn seed_installed_provider_version(&self, provider_version: Option<String>) {
        let provider_version = provider_version.and_then(|version| normalize_nonempty(&version));
        let mut state = self.inner.state.lock().await;
        state.installed_provider_version = provider_version.clone();
        state.provider_version = provider_version;
        if state.compact_command_advertised.is_some() {
            state.compaction_capability = claude_compaction_capability(
                state.compact_command_advertised,
                state.provider_version.as_deref(),
            );
        }
    }

    pub async fn shutdown(self) {
        self.inner.shutdown().await;
    }
}

struct ActiveTurn {
    id: u64,
    owner: ClaudeTurnOwner,
    outcome_tx: Option<oneshot::Sender<TurnOutcome>>,
    interrupt_requested: bool,
    pending_ask_user_question: Option<PendingAskUserQuestionControl>,
    pending_exit_plan_mode: Option<PendingExitPlanModeControl>,
    quiesced_waiters: Vec<oneshot::Sender<()>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ClaudeTurnOwner {
    User,
    Compaction(protocol::CompactionOperationId),
}

struct PendingClaudeCompaction {
    request: BackendCompactionRequest,
    terminal_tx: Option<oneshot::Sender<BackendCompactionResult>>,
    timeout_cancel_tx: Option<oneshot::Sender<()>>,
    turn_id: u64,
    process_generation: u64,
    dispatched_at: std::time::Instant,
    write_completed: bool,
    boundary: Option<ClaudeCompactionBoundary>,
    compact_result: Option<String>,
    compact_error: Option<String>,
    terminal_result_seen: bool,
    result_is_error: bool,
    diagnostic: Option<String>,
}

#[derive(Clone)]
struct ClaudeCompactionBoundary {
    uuid: String,
    metrics: CompactionMetrics,
}

/// How the Claude backend disposed of a send: either it fully handled the
/// input (turn started, tool response answered, or a visible error emitted),
/// or it was busy with an already-active turn and did not consume the
/// message — the caller must requeue it.
enum ClaudeSendAdmission {
    Handled,
    Busy,
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
    resume_bootstrap_required: bool,
    ephemeral: bool,
    model: Option<String>,
    effort: Option<ClaudeEffort>,
    permission_mode: Option<String>,
    startup_mcp_config_json: Option<String>,
    steering_content: Option<String>,
    agent_identity: Option<AgentIdentity>,
    tool_policy: ToolPolicy,
    /// This session's inline skill plugin, owned for the whole session so a
    /// respawn or a post-turn restart reuses the same root. Dropping the state
    /// unlinks it.
    skill_plugin: Option<Arc<ClaudeSkillPlugin>>,
    /// `tyde-skills:<name>` for every skill Tyde materialized, checked against
    /// the CLI's `init` frame.
    expected_skills: Vec<String>,
    /// Bumped every time a process is about to start. A verification watchdog
    /// captures this at arm time and refuses to act if it no longer matches, so
    /// a timer left over from a killed process cannot kill its replacement.
    skill_verification_generation: u64,
    /// The live watchdog, cancelled the moment verification settles or the
    /// session shuts down.
    skill_watchdog: Option<JoinHandle<()>>,
    cumulative_usage: Option<Value>,
    cumulative_usage_complete: bool,
    conversation_bytes_total: u64,
    active_turn: Option<ActiveTurn>,
    compaction_capability: BackendCompactionCapability,
    compact_command_advertised: Option<bool>,
    installed_provider_version: Option<String>,
    provider_version: Option<String>,
    process_generation: u64,
    resume_bootstrap: Option<ClaudeResumeBootstrap>,
    resume_empty_result_generation: Option<u64>,
    pending_compaction: Option<PendingClaudeCompaction>,
    /// Set by `shutdown`. Blocks new turns (including CLI-initiated ones) and
    /// process respawn after the backend has been told to close.
    closing: bool,
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
            resume_bootstrap_required: true,
            ephemeral: false,
            model: None,
            effort: None,
            permission_mode: None,
            startup_mcp_config_json: None,
            steering_content: None,
            agent_identity: None,
            tool_policy: ToolPolicy::Unrestricted,
            skill_plugin: None,
            expected_skills: Vec::new(),
            skill_verification_generation: 0,
            skill_watchdog: None,
            cumulative_usage: None,
            cumulative_usage_complete: true,
            conversation_bytes_total: 0,
            active_turn: None,
            compaction_capability: BackendCompactionCapability::unknown(
                BackendCompactionUnknownReason::ProcessNotInitialized,
                None,
                BackendCompactionCapabilityEvidence::None,
            ),
            compact_command_advertised: None,
            installed_provider_version: None,
            provider_version: None,
            process_generation: 0,
            resume_bootstrap: None,
            resume_empty_result_generation: None,
            pending_compaction: None,
            closing: false,
            restart_process_after_turn: false,
            subagent_emitter: None,
            capacity_access: ClaudeCapacityAccess::Unknown,
            capacity_refresh_in_flight: false,
            capacity_report_emitted: false,
            authoritative_capacity_emitted: false,
        }
    }
}

struct ClaudeResumeBootstrap {
    generation: u64,
    fork_session: bool,
    completions: Vec<oneshot::Sender<Result<(), String>>>,
    quarantined_frames: usize,
}

struct ClaudeInner {
    /// Typed emitter enforcing protocol ordering (stream pairing, tool
    /// pairing, cancellation sequence). Every wire event — including
    /// session-control ones like `SessionStarted` / `Error` — goes
    /// through here; there is no raw `event_tx` fallback.
    emitter: Arc<TurnEmitter>,
    active_response: StdMutex<Option<(String, ResponseHandle)>>,
    state: Mutex<ClaudeState>,
    runtime: Mutex<Option<ClaudeProcessRuntime>>,
    turn_event_gate: Mutex<()>,
    task_tracker: StdMutex<ClaudeTaskTracker>,
    background_tasks: StdMutex<BackgroundTaskRegistry>,
    native_subagent_tasks: StdMutex<HashSet<String>>,
    /// Gates the first prompt on the CLI confirming this session's skills.
    /// A `watch` rather than a one-shot because a respawn re-arms it.
    skill_readiness: watch::Sender<ClaudeSkillReadiness>,
    /// Set when a turn was cancelled before its `init` frame arrived.
    ///
    /// Readiness deliberately stays `Pending` — the session did not fail, it
    /// simply never learned — but nothing may act on that pending state
    /// afterwards. The frame only ever arrives in response to the first user
    /// message, so once that turn is cancelled this process will never report
    /// one; the flag clears when the next process arms. Atomic because the
    /// hot-path predicate that reads it is synchronous.
    skill_verification_abandoned: std::sync::atomic::AtomicBool,
    /// Armed when a background task reports a terminal status on the root
    /// stream, because that is what makes the CLI wake the model and run a
    /// turn no user message initiated. Adoption of such a turn requires this
    /// token, so a stray frame arriving after a `result` can never open one.
    /// Atomic because the reader consults it synchronously per frame.
    pending_cli_wake: std::sync::atomic::AtomicBool,
    /// True while provider-owned work can continue after the foreground turn
    /// that launched it has ended. Typing is the public activity contract, so
    /// a foreground `result` must not make the agent appear idle while this is
    /// set.
    background_work_active: std::sync::atomic::AtomicBool,
    typing_active: std::sync::atomic::AtomicBool,
}

struct BackgroundTaskRegistry {
    owner_active: bool,
    entries: HashMap<String, BackgroundTaskEntry>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BackgroundTaskStatus {
    Running,
    Completed,
    Failed,
    Stopped,
    Unknown,
}

#[derive(Clone, Debug)]
struct BackgroundTaskState {
    task_id: String,
    description: Option<String>,
    status: BackgroundTaskStatus,
    summary: Option<String>,
    output_unavailable: Option<String>,
}

impl BackgroundTaskRegistry {
    fn active() -> Self {
        Self {
            owner_active: true,
            entries: HashMap::new(),
        }
    }
}

/// Whether the CLI has confirmed the skills Tyde materialized for this session.
#[derive(Clone, Debug, PartialEq, Eq)]
enum ClaudeSkillReadiness {
    /// The session materialized no skills; nothing to confirm.
    NotRequired,
    /// A process is starting and its `init` frame has not been seen yet.
    Pending,
    /// Every materialized skill was reported loaded.
    Ready,
    /// The session is short at least one skill, or could not be confirmed to
    /// have them. It keeps running — this message is the notice the user gets,
    /// not a startup error.
    Degraded(String),
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
    async fn shutdown(mut self) {
        if let Err(err) = self.stdin.lock().await.shutdown().await {
            tracing::warn!("Failed to close Claude stdin for graceful shutdown: {err}");
        }
        let mut child = self.child.lock().await;
        if let Some(process) = child.as_mut() {
            let graceful = tokio::time::timeout(Duration::from_secs(5), process.wait()).await;
            eprintln!(
                "TYDE CLAUDE CLEANUP process_group_final_kill graceful_wait={:?}",
                graceful
                    .as_ref()
                    .map(|result| result.as_ref().map(|status| status.code()))
            );
            // `claude` may exit after closing stdin while a provider-owned
            // background command remains in its process group. Always signal
            // the group after the graceful wait so no descendant survives a
            // session shutdown, including a task_started frame that raced the
            // stop_task snapshot.
            let _ = process.kill().await;
        }
        *child = None;
        drop(child);
        if tokio::time::timeout(Duration::from_secs(2), &mut self.stdout_task)
            .await
            .is_err()
        {
            self.stdout_task.abort();
        }
        self.stderr_task.abort();
    }

    async fn kill(mut self) {
        let mut child = self.child.lock().await;
        if let Some(child) = child.as_mut() {
            let _ = child.kill().await;
        }
        *child = None;
        drop(child);
        if tokio::time::timeout(Duration::from_secs(2), &mut self.stdout_task)
            .await
            .is_err()
        {
            self.stdout_task.abort();
        }
        self.stderr_task.abort();
    }

    fn abort_readers(&self) {
        self.stdout_task.abort();
        self.stderr_task.abort();
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
    fn disarm(&mut self) -> oneshot::Sender<()> {
        self.0
            .take()
            .expect("Claude startup cancellation guard already disarmed")
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
    compact_result: Option<String>,
    #[serde(default)]
    compact_error: Option<String>,
    #[serde(default)]
    compact_metadata: Option<Value>,
    #[serde(default)]
    uuid: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    task_id: Option<String>,
    #[serde(default)]
    task_type: Option<String>,
    #[serde(default)]
    tool_use_id: Option<String>,
    #[serde(default)]
    output_file: Option<String>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    workflow_name: Option<String>,
    /// Partial-update object on `task_updated` frames. Only `status` is
    /// consumed; the CLI also sends fields like `end_time`.
    #[serde(default)]
    patch: Option<ClaudeTaskPatch>,
    /// Aggregate usage on `task_progress` frames.
    #[serde(default)]
    usage: Option<ClaudeTaskUsage>,
    /// Per-workflow-agent delta events on `task_progress` frames. Each
    /// entry is parsed individually into `ClaudeWorkflowAgentDelta` so
    /// one malformed delta is surfaced and skipped without losing the
    /// rest of the frame.
    #[serde(default)]
    workflow_progress: Option<Vec<Value>>,
    #[serde(default)]
    attempt: Option<u64>,
    #[serde(default)]
    max_retries: Option<u64>,
    #[serde(default)]
    retry_delay_ms: Option<u64>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_status: Option<u16>,
}

#[derive(Debug, Deserialize)]
struct ClaudeTaskPatch {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    output_file: Option<String>,
    #[serde(default)]
    path: Option<String>,
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

/// Does this frame end the CLI's turn?
///
/// Used only while skill verification is still pending: a turn that reaches its
/// `result` without ever emitting an `init` frame is never going to emit one, so
/// the reader stops holding its output and says the skills were unconfirmed.
fn claude_frame_is_turn_terminal(value: &Value) -> bool {
    value.get("type").and_then(Value::as_str) == Some("result")
}

/// Put held-back frames back at the front of the queue, in arrival order.
///
/// `after` is the frame that ended the hold; it is requeued *behind* the frames
/// it followed so the replay preserves true arrival order. Every exit from the
/// hold goes through here — held frames are model output, and there is no
/// outcome, skill gap included, that makes discarding them right.
fn release_held_frames(
    held_back: &mut Vec<Value>,
    held_back_bytes: &mut usize,
    queue: &mut std::collections::VecDeque<Value>,
    after: Option<Value>,
) {
    if let Some(value) = after {
        queue.push_front(value);
    }
    for held in held_back.drain(..).rev() {
        queue.push_front(held);
    }
    *held_back_bytes = 0;
}

/// Skills reported by a `system`/`init` frame.
///
/// `None` means this is not an `init` frame. `Some(Ok(None))` means the frame
/// carried no `skills` field at all, and `Some(Err(_))` means it carried one
/// Tyde cannot read — both mean the session's skills are unconfirmed, which is
/// reported rather than assumed either way. Parsed straight from the JSON rather
/// than through `ClaudeSystemFrame` so a malformed `skills` field cannot make
/// the whole frame unparseable and thus invisible to the check.
fn claude_init_frame_skills(value: &Value) -> Option<Result<Option<Vec<String>>, String>> {
    if value.get("type").and_then(Value::as_str) != Some("system")
        || value.get("subtype").and_then(Value::as_str) != Some("init")
    {
        return None;
    }
    let Some(raw) = value.get("skills") else {
        return Some(Ok(None));
    };
    let Some(items) = raw.as_array() else {
        return Some(Err(
            "Claude's init frame reported a 'skills' field that is not an array, so Tyde \
             cannot confirm the session's skills were loaded"
                .to_string(),
        ));
    };
    let mut names = Vec::with_capacity(items.len());
    for item in items {
        let Some(name) = item.as_str() else {
            return Some(Err(
                "Claude's init frame reported a 'skills' entry that is not a string, so Tyde \
                 cannot confirm the session's skills were loaded"
                    .to_string(),
            ));
        };
        names.push(name.to_string());
    }
    Some(Ok(Some(names)))
}

fn parse_claude_system_frame(value: &Value) -> Result<ClaudeSystemFrame, String> {
    serde_json::from_value::<ClaudeSystemFrame>(value.clone())
        .map_err(|err| format!("invalid Claude system frame: {err}; value={value}"))
}

fn claude_live_compaction_observation(
    value: &Value,
    system: &ClaudeSystemFrame,
    fallback_session_id: Option<&str>,
) -> Option<BackendObservedCompaction> {
    let boundary_uuid = system
        .uuid
        .clone()
        .or_else(|| value.get("uuid").and_then(Value::as_str).map(str::to_owned))?;
    let session_id = system
        .session_id
        .clone()
        .or_else(|| fallback_session_id.map(str::to_owned))
        .or_else(|| {
            value
                .get("session_id")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })?;
    let metadata = system.compact_metadata.as_ref();
    let trigger = metadata
        .and_then(|metadata| metadata.get("trigger"))
        .and_then(Value::as_str)
        .unwrap_or("auto");
    let trigger = if trigger == "manual" {
        CompactionTrigger::BackendObservedManual
    } else {
        CompactionTrigger::BackendAutomatic
    };
    Some(BackendObservedCompaction {
        observation_id: super::compaction::stable_observation_id(
            "claude",
            &session_id,
            &boundary_uuid,
        ),
        trigger,
        method: if trigger == CompactionTrigger::BackendAutomatic {
            CompactionMethod::BackendAutomatic
        } else {
            CompactionMethod::NativeTextCommand
        },
        provider_session_id: Some(SessionId(session_id)),
        metrics: claude_compaction_metrics(metadata),
        source: BackendCompactionObservationSource::ClaudeBoundary { boundary_uuid },
        user_focus: metadata
            .and_then(|metadata| metadata.get("user_context"))
            .and_then(Value::as_str)
            .and_then(normalize_nonempty)
            .map(|text| BackendCompactionUserFocus {
                text,
                provenance: BackendCompactionUserFocusProvenance::ProviderEcho,
            }),
    })
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
        if let Some(reasoning) = self
            .result_reasoning
            .as_ref()
            .filter(|text| contains_non_whitespace(text))
        {
            return Some(reasoning.clone());
        }
        if contains_non_whitespace(&self.streamed_reasoning) {
            return Some(self.streamed_reasoning.clone());
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

#[derive(Default)]
struct ClaudeTerminalPhaseOptions {
    turn_id: u64,
    conversation_history_bytes: u64,
    known_context_window: Option<u64>,
    model_hint: Option<String>,
    turn_usage: Option<ClaudeTurnUsage>,
    cancelled: bool,
}

#[derive(Debug, Clone, Default)]
struct ClaudeMessageUsage {
    request: Option<Value>,
    turn: Option<Value>,
    cumulative: Option<Value>,
}

fn claude_message_token_usage(usage: ClaudeMessageUsage) -> Option<MessageTokenUsage> {
    let request = usage.request.and_then(claude_token_usage);
    let turn = usage.turn.and_then(claude_token_usage);
    let cumulative = usage.cumulative.and_then(claude_token_usage);
    if request.is_none() && turn.is_none() && cumulative.is_none() {
        return None;
    }
    let turn_reported = turn.is_some();
    let known = |usage| TokenUsageScope::Known {
        usage: Box::new(usage),
    };
    let unavailable = |reason| TokenUsageScope::Unavailable { reason };
    Some(MessageTokenUsage {
        request: request
            .map(known)
            .unwrap_or_else(|| unavailable(TokenUsageUnavailableReason::BackendDidNotReport)),
        turn: turn
            .map(known)
            .unwrap_or_else(|| unavailable(TokenUsageUnavailableReason::BackendDidNotReport)),
        cumulative: cumulative.map(known).unwrap_or_else(|| {
            unavailable(if turn_reported {
                TokenUsageUnavailableReason::ProviderScopeAmbiguous
            } else {
                TokenUsageUnavailableReason::BackendDidNotReport
            })
        }),
    })
}

fn claude_token_usage(value: Value) -> Option<TokenUsage> {
    serde_json::from_value(value)
        .map_err(|error| tracing::warn!(%error, "dropping invalid Claude token usage"))
        .ok()
}

fn claude_tool_use_data(value: Value, default_content_offset: u32) -> Option<ToolUseData> {
    let tool_call_id = value
        .get("tool_call_id")
        .or_else(|| value.get("id"))
        .and_then(Value::as_str)?
        .trim()
        .to_owned();
    if tool_call_id.is_empty() {
        return None;
    }
    let name = value.get("name").and_then(Value::as_str)?.trim().to_owned();
    if name.is_empty() {
        return None;
    }
    Some(ToolUseData {
        tool_call_id,
        name,
        arguments: value.get("arguments").cloned().unwrap_or(Value::Null),
        content_offset: value
            .get("content_offset")
            .and_then(Value::as_u64)
            .and_then(|offset| u32::try_from(offset).ok())
            .or(Some(default_content_offset)),
    })
}

enum ClaudeHistoryReplayItem {
    Message(Value),
    ToolRequest(ClaudeToolCall),
    ToolExecutionCompleted(ClaudeReplayToolExecution),
    Compaction(BackendObservedCompaction),
}

fn claude_replay_requires_resume_bootstrap(items: &[ClaudeHistoryReplayItem]) -> bool {
    let Some(ClaudeHistoryReplayItem::Message(message)) = items
        .iter()
        .rev()
        .find(|item| matches!(item, ClaudeHistoryReplayItem::Message(_)))
    else {
        return true;
    };
    let provider_quiescent_after_interrupt = message
        .get("sender")
        .and_then(Value::as_str)
        .is_some_and(|sender| sender.eq_ignore_ascii_case("user"))
        && message
            .get("content")
            .and_then(Value::as_str)
            .is_some_and(|content| {
                matches!(
                    content.trim(),
                    "[Request interrupted by user]" | "[Request interrupted by user for tool use]"
                )
            });
    !provider_quiescent_after_interrupt
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
    /// Root of this session's inline skill plugin. Rebuilt into every process
    /// config from `ClaudeState`, so a respawned or forked process points at
    /// the same root the session already materialized. `None` for SSH sessions
    /// and for sessions with no exposable skill.
    skill_plugin_root: Option<String>,
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

/// Identity a skill failure was decided against.
///
/// A failure is decided at one moment and committed at another, with awaits in
/// between; a cancel can land in that gap. Carrying the process generation and
/// the exact turn the decision was made for lets the commit revalidate rather
/// than assume, so a stale decision can never settle a turn it was not about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SkillFailureTarget {
    generation: u64,
    turn_id: Option<u64>,
}

/// What a committed skill gap hands back for the caller to finish.
///
/// Only the watchdog: the gap does not touch the turn's outcome sender, because
/// a turn is not failed by a skill the session could not expose.
struct CommittedSkillFailure {
    watchdog: Option<JoinHandle<()>>,
}

impl ClaudeInner {
    async fn execute_arc(this: Arc<Self>, command: SessionCommand) -> Result<(), String> {
        match command {
            SessionCommand::SendMessage { message, images } => {
                match Self::send_message(this.clone(), message, images, None).await? {
                    ClaudeSendAdmission::Handled => Ok(()),
                    // See `send_message_payload`: this command path cannot
                    // hand the message back for requeueing.
                    ClaudeSendAdmission::Busy => {
                        this.emit_error(
                            "Claude is busy with another turn; the message was not delivered.",
                        );
                        Err("Claude backend was busy on a path that cannot requeue".to_string())
                    }
                }
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
    ) -> Result<ClaudeSendAdmission, String> {
        if let Some(tool_response) = tool_response {
            if this
                .answer_pending_tool_response(tool_response, message.clone())
                .await?
            {
                return Ok(ClaudeSendAdmission::Handled);
            }
            this.emit_error("No matching pending tool request is waiting for that response.");
            return Ok(ClaudeSendAdmission::Handled);
        }

        Ok(this.start_turn(message, images).await)
    }

    /// Start a user turn, or report that the backend is busy with a turn it
    /// already has in flight. On `Busy` nothing is emitted or consumed — the
    /// caller retains the message and requeues it above the backend.
    async fn start_turn(
        self: Arc<Self>,
        message: String,
        images: Option<Vec<ImageAttachment>>,
    ) -> ClaudeSendAdmission {
        let images = images.unwrap_or_default();
        let input_bytes = estimate_turn_input_bytes(&message, &images);
        let (turn_id, conversation_history_bytes, model_hint, ephemeral, outcome_rx) = {
            let mut state = self.state.lock().await;
            if state.closing {
                drop(state);
                self.emit_error("Claude backend is shutting down; the message was not sent.");
                return ClaudeSendAdmission::Handled;
            }
            if state.active_turn.is_some() {
                return ClaudeSendAdmission::Busy;
            }

            let turn_id = CLAUDE_TURN_COUNTER.fetch_add(1, Ordering::Relaxed);
            let (outcome_tx, outcome_rx) = oneshot::channel();
            state.active_turn = Some(ActiveTurn {
                id: turn_id,
                owner: ClaudeTurnOwner::User,
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

        // The user bubble is emitted only once the turn is admitted, so a
        // busy hand-back (which redispatches later) can never duplicate it and
        // the chat never shows a message that was not delivered.
        self.emit_user_message_added(&message, (!images.is_empty()).then_some(images.as_slice()));
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
        ClaudeSendAdmission::Handled
    }

    async fn begin_compaction(
        self: Arc<Self>,
        request: BackendCompactionRequest,
    ) -> BackendCompactionStart {
        let focus = match claude_compaction_focus(&request) {
            Err(()) => {
                return BackendCompactionStart::NotDispatched {
                    reason: BackendCompactionNotDispatchedReason::InvalidFocus,
                    fallback_safe: false,
                };
            }
            Ok(focus) => focus,
        };
        let (turn_id, terminal_rx, timeout_cancel_rx) = {
            let mut state = self.state.lock().await;
            if state.closing {
                return BackendCompactionStart::NotDispatched {
                    reason: BackendCompactionNotDispatchedReason::BackendClosed,
                    fallback_safe: false,
                };
            }
            if state.pending_compaction.is_some() {
                return BackendCompactionStart::Deferred {
                    reason: BackendCompactionDeferredReason::AnotherCompactionActive,
                };
            }
            if state.active_turn.is_some() {
                return BackendCompactionStart::Deferred {
                    reason: BackendCompactionDeferredReason::ActiveTurn,
                };
            }
            if let Some(start) =
                super::compaction::not_dispatched_for_capability(&state.compaction_capability)
            {
                return start;
            }
            let turn_id = CLAUDE_TURN_COUNTER.fetch_add(1, Ordering::Relaxed);
            let (terminal_tx, terminal_rx) = oneshot::channel();
            let (timeout_cancel_tx, timeout_cancel_rx) = oneshot::channel();
            state.active_turn = Some(ActiveTurn {
                id: turn_id,
                owner: ClaudeTurnOwner::Compaction(request.operation_id.clone()),
                outcome_tx: None,
                interrupt_requested: false,
                pending_ask_user_question: None,
                pending_exit_plan_mode: None,
                quiesced_waiters: Vec::new(),
            });
            let process_generation = state.process_generation;
            state.pending_compaction = Some(PendingClaudeCompaction {
                request: request.clone(),
                terminal_tx: Some(terminal_tx),
                timeout_cancel_tx: Some(timeout_cancel_tx),
                turn_id,
                process_generation,
                dispatched_at: std::time::Instant::now(),
                write_completed: false,
                boundary: None,
                compact_result: None,
                compact_error: None,
                terminal_result_seen: false,
                result_is_error: false,
                diagnostic: None,
            });
            (turn_id, terminal_rx, timeout_cancel_rx)
        };

        if let Err(error) = self.ensure_process_ready().await {
            self.abandon_undispatched_compaction(turn_id).await;
            return BackendCompactionStart::NotDispatched {
                reason: BackendCompactionNotDispatchedReason::CapabilityUnknown(
                    BackendCompactionUnknownReason::CapabilityProbeFailed(error),
                ),
                fallback_safe: false,
            };
        }
        {
            let mut state = self.state.lock().await;
            if let Some(start) =
                super::compaction::not_dispatched_for_capability(&state.compaction_capability)
            {
                drop(state);
                self.abandon_undispatched_compaction(turn_id).await;
                return start;
            }
            let process_generation = state.process_generation;
            if let Some(pending) = state.pending_compaction.as_mut() {
                pending.process_generation = process_generation;
            }
        }

        let prompt = focus
            .as_deref()
            .map(|focus| format!("/compact {focus}"))
            .unwrap_or_else(|| "/compact".to_string());
        let input = build_stream_json_user_message(&prompt, &[]);
        if let Err(error) = self.write_process_json_line(&input).await {
            let result = self
                .finish_compaction(
                    turn_id,
                    BackendCompactionFailureKind::TransportClosed,
                    Some(format!(
                        "Claude compaction write may have reached the provider: {error}"
                    )),
                    true,
                )
                .await
                .unwrap_or_else(|| {
                    dispatch_uncertain_claude_result(request.operation_id.clone(), error)
                });
            return BackendCompactionStart::DispatchUncertain(Box::new(result));
        }
        {
            let mut state = self.state.lock().await;
            if let Some(pending) = state.pending_compaction.as_mut()
                && pending.turn_id == turn_id
            {
                pending.dispatched_at = std::time::Instant::now();
                pending.write_completed = true;
            }
        }
        let timeout_inner = Arc::clone(&self);
        tokio::spawn(async move {
            tokio::select! {
                _ = tokio::time::sleep(CLAUDE_COMPACTION_TIMEOUT) => {
                    let _ = timeout_inner
                        .finish_compaction(
                            turn_id,
                            BackendCompactionFailureKind::TimedOut,
                            Some("Timed out waiting for Claude compaction to quiesce".to_string()),
                            false,
                        )
                        .await;
                }
                _ = timeout_cancel_rx => {}
            }
        });
        BackendCompactionStart::Accepted(super::BackendAcceptedCompaction {
            operation_id: request.operation_id,
            terminal: terminal_rx,
        })
    }

    async fn abandon_undispatched_compaction(&self, turn_id: u64) {
        let waiters = {
            let mut state = self.state.lock().await;
            if state
                .pending_compaction
                .as_ref()
                .is_some_and(|pending| pending.turn_id == turn_id)
            {
                state.pending_compaction.take();
            }
            match state.active_turn.as_ref() {
                Some(active)
                    if active.id == turn_id
                        && matches!(active.owner, ClaudeTurnOwner::Compaction(_)) =>
                {
                    state
                        .active_turn
                        .take()
                        .map(|active| active.quiesced_waiters)
                        .unwrap_or_default()
                }
                _ => Vec::new(),
            }
        };
        notify_turn_quiesced(waiters);
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
                        ClaudeTerminalPhaseOptions {
                            turn_id,
                            conversation_history_bytes,
                            known_context_window,
                            model_hint: result_model_hint.or(model_hint),
                            turn_usage,
                            cancelled: false,
                        },
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
                    ClaudeTerminalPhaseOptions {
                        turn_id,
                        conversation_history_bytes,
                        known_context_window,
                        model_hint: None,
                        turn_usage,
                        cancelled: true,
                    },
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
                        ClaudeTerminalPhaseOptions {
                            turn_id,
                            conversation_history_bytes,
                            known_context_window,
                            model_hint: None,
                            turn_usage,
                            cancelled: false,
                        },
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

    fn arm_cli_wake(&self) {
        self.pending_cli_wake
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    fn take_pending_cli_wake(&self) -> bool {
        self.pending_cli_wake
            .swap(false, std::sync::atomic::Ordering::Relaxed)
    }

    fn has_pending_cli_wake(&self) -> bool {
        self.pending_cli_wake
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    fn set_background_work_active(&self, active: bool) -> bool {
        self.background_work_active
            .swap(active, std::sync::atomic::Ordering::Relaxed)
    }

    fn emit_idle_if_quiescent(&self) {
        if !self
            .background_work_active
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            self.emit_typing_status(false);
        }
    }

    async fn emit_idle_if_no_active_turn(&self) {
        let turn_active = self
            .state
            .lock()
            .await
            .active_turn
            .as_ref()
            .is_some_and(|turn| {
                !matches!(turn.owner, ClaudeTurnOwner::User) || turn.outcome_tx.is_some()
            });
        if !turn_active {
            self.emit_idle_if_quiescent();
        }
    }

    /// Wait until no turn is installed, i.e. the previous turn's finalizer has
    /// run. The finalizer is a spawned task, so a turn that has already
    /// produced its `result` stays installed for a short window afterwards —
    /// and the CLI can deliver an entire wake burst inside that window.
    /// Retrying per frame is not enough, because every frame of the burst can
    /// land before the hand-off completes. Returns false on timeout.
    /// Safe to await while holding `turn_event_gate`: the finalizer never
    /// takes that gate.
    async fn await_active_turn_quiesced(&self, wait: Duration) -> bool {
        let quiesced_rx = {
            let mut state = self.state.lock().await;
            let Some(active) = state.active_turn.as_mut() else {
                return true;
            };
            let (quiesced_tx, quiesced_rx) = oneshot::channel();
            active.quiesced_waiters.push(quiesced_tx);
            quiesced_rx
        };
        matches!(
            tokio::time::timeout(wait, quiesced_rx).await,
            Ok(Ok(())) | Err(_)
        ) && self.state.lock().await.active_turn.is_none()
    }

    /// Open a turn for output the Claude CLI produced on its own initiative,
    /// with no pending user message — the CLI wakes the model when a
    /// background task finishes, and that turn is the only way the model gets
    /// to act on the result. Mirrors the scaffolding `start_turn` builds
    /// (allocate a turn id, emit typing + stream start, spawn the finalizer
    /// that awaits the outcome) so the unsolicited turn flows through the
    /// exact same completion path as a user-initiated one. It deliberately
    /// emits no user bubble. Returns `None` if a turn is somehow already
    /// active, or if the backend is closing.
    async fn begin_cli_initiated_turn(self: &Arc<Self>) -> Option<u64> {
        let (turn_id, ephemeral, conversation_history_bytes, model_hint, outcome_rx) = {
            let mut state = self.state.lock().await;
            if state.closing || state.active_turn.is_some() {
                return None;
            }
            let turn_id = CLAUDE_TURN_COUNTER.fetch_add(1, Ordering::Relaxed);
            let (outcome_tx, outcome_rx) = oneshot::channel();
            state.active_turn = Some(ActiveTurn {
                id: turn_id,
                owner: ClaudeTurnOwner::User,
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

    async fn observe_compaction_frame(&self, turn_id: u64, value: &Value) {
        tracing::warn!(
            turn_id,
            frame = %value,
            "Claude compaction diagnostic frame"
        );
        if let Some(session_id) = value.get("session_id").and_then(Value::as_str) {
            let current_session_id = self.state.lock().await.session_id.clone();
            if current_session_id
                .as_deref()
                .is_some_and(|current| current != session_id)
            {
                return;
            }
            if current_session_id.is_none() {
                self.set_session_id(session_id.to_string()).await;
            }
        }
        let message_type = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match message_type {
            "system" => {
                let Ok(system) = parse_claude_system_frame(value) else {
                    return;
                };
                match system.event() {
                    ClaudeSystemEvent::Status => {
                        let mut progress = None;
                        let mut state = self.state.lock().await;
                        let process_generation = state.process_generation;
                        let session_matches = system
                            .session_id
                            .as_deref()
                            .or_else(|| value.get("session_id").and_then(Value::as_str))
                            .is_none_or(|frame_session| {
                                state.session_id.as_deref() == Some(frame_session)
                            });
                        let Some(pending) = state.pending_compaction.as_mut() else {
                            return;
                        };
                        if pending.turn_id != turn_id
                            || pending.process_generation != process_generation
                            || !pending.write_completed
                            || !session_matches
                        {
                            return;
                        }
                        if system.status.as_deref() == Some("compacting") {
                            progress = Some(BackendCompactionProgress {
                                operation_id: pending.request.operation_id.clone(),
                                stage: CompactionStage::Compacting,
                                elapsed_ms: Some(pending.dispatched_at.elapsed().as_millis() as u64),
                            });
                        }
                        if let Some(compact_result) = system.compact_result {
                            pending.compact_result = Some(compact_result);
                            pending.compact_error = system.compact_error;
                        }
                        drop(state);
                        if let Some(progress) = progress {
                            self.emitter
                                .compaction_event(&BackendCompactionEvent::Progress(progress));
                        }
                    }
                    ClaudeSystemEvent::CompactBoundary => {
                        let trigger = system
                            .compact_metadata
                            .as_ref()
                            .and_then(|metadata| metadata.get("trigger"))
                            .and_then(Value::as_str)
                            .unwrap_or("auto");
                        if trigger != "manual" {
                            let session_id = {
                                let state = self.state.lock().await;
                                state.session_id.clone()
                            };
                            if let Some(observation) = claude_live_compaction_observation(
                                value,
                                &system,
                                session_id.as_deref(),
                            ) {
                                self.emitter
                                    .compaction_event(&BackendCompactionEvent::Observed(Box::new(
                                        observation,
                                    )));
                            }
                            return;
                        }
                        let Some(uuid) = system.uuid.clone().or_else(|| {
                            value.get("uuid").and_then(Value::as_str).map(str::to_owned)
                        }) else {
                            return;
                        };
                        let mut state = self.state.lock().await;
                        let process_generation = state.process_generation;
                        let session_matches = system
                            .session_id
                            .as_deref()
                            .or_else(|| value.get("session_id").and_then(Value::as_str))
                            .is_none_or(|frame_session| {
                                state.session_id.as_deref() == Some(frame_session)
                            });
                        let Some(pending) = state.pending_compaction.as_mut() else {
                            return;
                        };
                        if pending.turn_id != turn_id
                            || pending.process_generation != process_generation
                            || !pending.write_completed
                            || !session_matches
                        {
                            return;
                        }
                        pending.boundary = Some(ClaudeCompactionBoundary {
                            uuid,
                            metrics: claude_compaction_metrics(system.compact_metadata.as_ref()),
                        });
                    }
                    ClaudeSystemEvent::Init
                    | ClaudeSystemEvent::TaskStarted
                    | ClaudeSystemEvent::TaskProgress
                    | ClaudeSystemEvent::TaskNotification
                    | ClaudeSystemEvent::BackgroundTasksChanged
                    | ClaudeSystemEvent::TaskUpdated
                    | ClaudeSystemEvent::ThinkingTokens
                    | ClaudeSystemEvent::ApiRetry
                    | ClaudeSystemEvent::Unknown(_) => {}
                }
            }
            "assistant" => {
                let diagnostic = value
                    .get("message")
                    .and_then(extract_text_from_message)
                    .and_then(|diagnostic| normalize_nonempty(&diagnostic));
                if let Some(diagnostic) = diagnostic {
                    let mut state = self.state.lock().await;
                    if let Some(pending) = state.pending_compaction.as_mut()
                        && pending.turn_id == turn_id
                    {
                        pending.diagnostic = Some(diagnostic);
                    }
                }
            }
            "result" => {
                let mut state = self.state.lock().await;
                if let Some(pending) = state.pending_compaction.as_mut()
                    && pending.turn_id == turn_id
                {
                    pending.terminal_result_seen = true;
                    pending.result_is_error = value
                        .get("is_error")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                        || value.get("subtype").and_then(Value::as_str) == Some("error");
                    if pending.diagnostic.is_none() {
                        pending.diagnostic = extract_result_error(value);
                    }
                }
            }
            _ => {}
        }
    }

    async fn finish_compaction(
        &self,
        turn_id: u64,
        forced_failure_kind: BackendCompactionFailureKind,
        forced_failure: Option<String>,
        dispatch_uncertain: bool,
    ) -> Option<BackendCompactionResult> {
        let (result, observation, terminal_tx, timeout_cancel_tx, waiters) = {
            let mut state = self.state.lock().await;
            let active_matches = state.active_turn.as_ref().is_some_and(|active| {
                active.id == turn_id && matches!(active.owner, ClaudeTurnOwner::Compaction(_))
            });
            if !active_matches {
                return None;
            }
            let mut pending = state.pending_compaction.take()?;
            if pending.turn_id != turn_id {
                state.pending_compaction = Some(pending);
                return None;
            }
            let interrupted = state
                .active_turn
                .as_ref()
                .is_some_and(|active| active.interrupt_requested);
            let boundary = pending.boundary.as_ref();
            let typed_failed = pending.compact_result.as_deref() == Some("failed");
            let semantic_failure = forced_failure
                .or_else(|| interrupted.then(|| "Claude compaction was interrupted".to_string()))
                .or_else(|| pending.compact_error.clone())
                .or_else(|| {
                    pending.result_is_error.then(|| {
                        pending
                            .diagnostic
                            .clone()
                            .unwrap_or_else(|| "Claude returned an error result".to_string())
                    })
                })
                .or_else(|| {
                    typed_failed.then(|| {
                        pending
                            .diagnostic
                            .clone()
                            .unwrap_or_else(|| "Claude reported compaction failure".to_string())
                    })
                })
                .or_else(|| {
                    (!pending.terminal_result_seen)
                        .then(|| "Claude compaction did not reach a terminal result".to_string())
                })
                .or_else(|| {
                    boundary.is_none().then(|| {
                        "Claude returned a result without a manual compact boundary".to_string()
                    })
                });
            tracing::warn!(
                turn_id,
                terminal_result_seen = pending.terminal_result_seen,
                result_is_error = pending.result_is_error,
                compact_result = ?pending.compact_result,
                compact_error = ?pending.compact_error,
                diagnostic = ?pending.diagnostic,
                boundary = ?pending.boundary.as_ref().map(|boundary| &boundary.uuid),
                semantic_failure = ?semantic_failure,
                "Claude compaction terminal diagnostic"
            );
            let dispatch = if dispatch_uncertain {
                BackendCompactionDispatchState::MayHaveReachedProvider
            } else {
                BackendCompactionDispatchState::Accepted
            };
            let mutation = if boundary.is_some() {
                BackendCompactionMutationState::Completed
            } else if dispatch_uncertain
                || matches!(
                    forced_failure_kind,
                    BackendCompactionFailureKind::TimedOut
                        | BackendCompactionFailureKind::TransportClosed
                )
            {
                BackendCompactionMutationState::MayHaveMutated
            } else {
                BackendCompactionMutationState::NotObserved
            };
            let metrics = boundary
                .map(|boundary| boundary.metrics.clone())
                .unwrap_or_default();
            let post_context_tokens = metrics
                .after_tokens
                .map(PostCompactionTokenCount::Trusted)
                .unwrap_or(PostCompactionTokenCount::Unknown);
            let outcome = if let Some(message) = semantic_failure {
                Err(BackendCompactionFailure {
                    kind: if interrupted {
                        BackendCompactionFailureKind::Interrupted
                    } else if typed_failed || pending.result_is_error {
                        BackendCompactionFailureKind::ProviderFailed
                    } else {
                        forced_failure_kind
                    },
                    message,
                })
            } else {
                Ok(BackendCompactionSuccess {
                    mechanism: CompactionMethod::NativeTextCommand,
                })
            };
            let result = BackendCompactionResult {
                operation_id: pending.request.operation_id.clone(),
                dispatch,
                mutation,
                outcome,
                provider_session_id: state.session_id.clone().map(SessionId),
                metrics,
                post_context_tokens,
                evidence: BackendCompactionTerminalEvidence::Claude {
                    session_id: state.session_id.clone(),
                    boundary_uuid: boundary.map(|boundary| boundary.uuid.clone()),
                    compact_result: pending.compact_result.clone(),
                    terminal_result_seen: pending.terminal_result_seen,
                },
            };
            let observation = boundary.and_then(|boundary| {
                let session_id = state.session_id.clone()?;
                Some(BackendObservedCompaction {
                    observation_id: super::compaction::stable_observation_id(
                        "claude",
                        &session_id,
                        &boundary.uuid,
                    ),
                    trigger: CompactionTrigger::BackendObservedManual,
                    method: CompactionMethod::NativeTextCommand,
                    provider_session_id: Some(SessionId(session_id)),
                    metrics: boundary.metrics.clone(),
                    source: BackendCompactionObservationSource::ClaudeBoundary {
                        boundary_uuid: boundary.uuid.clone(),
                    },
                    user_focus: pending.request.focus.clone().map(|text| {
                        BackendCompactionUserFocus {
                            text,
                            provenance: BackendCompactionUserFocusProvenance::TydeRequest,
                        }
                    }),
                })
            });
            let terminal_tx = pending.terminal_tx.take();
            let timeout_cancel_tx = pending.timeout_cancel_tx.take();
            let waiters = state
                .active_turn
                .take()
                .map(|active| active.quiesced_waiters)
                .unwrap_or_default();
            (result, observation, terminal_tx, timeout_cancel_tx, waiters)
        };
        if let Some(timeout_cancel_tx) = timeout_cancel_tx {
            let _ = timeout_cancel_tx.send(());
        }
        if let Some(observation) = observation {
            self.emitter
                .compaction_event(&BackendCompactionEvent::Observed(Box::new(observation)));
        }
        if let Some(terminal_tx) = terminal_tx {
            let _ = terminal_tx.send(result.clone());
        }
        notify_turn_quiesced(waiters);
        if self.take_restart_process_after_turn().await {
            self.shutdown_process().await;
        }
        Some(result)
    }

    async fn active_turn_owner(&self, turn_id: u64) -> Option<ClaudeTurnOwner> {
        let state = self.state.lock().await;
        state
            .active_turn
            .as_ref()
            .filter(|active| active.id == turn_id)
            .map(|active| active.owner.clone())
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
        let stdin = {
            let runtime = self.runtime.lock().await;
            runtime
                .as_ref()
                .map(|runtime| Arc::clone(&runtime.stdin))
                .ok_or_else(|| {
                    TurnStartError::Failed("Claude CLI process is not running".to_string())
                })?
        };
        let (resume_bootstrap_generation, resume_bootstrap_rx, written) = {
            // Keep the bootstrap state locked through the stdin write so the
            // resume terminal cannot race past waiter registration.
            let mut state = self.state.lock().await;
            let (generation, receiver) = if let Some(bootstrap) = state.resume_bootstrap.as_mut() {
                let (completion, receiver) = oneshot::channel();
                bootstrap.completions.push(completion);
                (Some(bootstrap.generation), Some(receiver))
            } else {
                (None, None)
            };
            let written = write_json_line_to_stdin(&stdin, &input_message).await;
            if written.is_ok() {
                state.resume_bootstrap_required = true;
            }
            (generation, receiver, written)
        };
        if let Err(error) = written {
            if let Some(generation) = resume_bootstrap_generation {
                self.fail_resume_bootstrap(generation, &error).await;
            }
            return Err(TurnStartError::Failed(error));
        }
        if let (Some(generation), Some(receiver)) =
            (resume_bootstrap_generation, resume_bootstrap_rx)
        {
            let inner = Arc::clone(self);
            tokio::spawn(async move {
                match tokio::time::timeout(CLAUDE_INITIALIZE_TIMEOUT, receiver).await {
                    Ok(Ok(Ok(()))) => {}
                    Ok(Ok(Err(error))) => {
                        tracing::warn!(generation, error, "Claude resume bootstrap failed");
                    }
                    Ok(Err(_)) => {
                        tracing::warn!(generation, "Claude resume bootstrap waiter closed");
                    }
                    Err(_) => {
                        let error = format!(
                            "Timed out waiting for Claude resume bootstrap generation {generation} to reach its next turn boundary"
                        );
                        inner.fail_resume_bootstrap(generation, &error).await;
                    }
                }
            });
        }
        self.watch_for_skill_verification().await;
        Ok(())
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
            if active.interrupt_requested {
                return Err("Claude asked a question after the turn was interrupted".to_string());
            }
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
        {
            let mut state = self.state.lock().await;
            let active = state
                .active_turn
                .as_mut()
                .ok_or_else(|| "Claude requested plan approval with no active turn".to_string())?;
            if active.interrupt_requested {
                return Err(
                    "Claude requested plan approval after the turn was interrupted".to_string(),
                );
            }
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
            SendMessageToolResponse::AskUserQuestion {
                tool_call_id,
                answer,
            } => {
                self.answer_pending_ask_user_question(tool_call_id, answer)
                    .await
            }
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
        tool_call_id: String,
        message: String,
    ) -> Result<bool, String> {
        let _turn_event_guard = self.turn_event_gate.lock().await;
        let (turn_id, pending) = {
            let state = self.state.lock().await;
            let Some(active) = state.active_turn.as_ref() else {
                return Ok(false);
            };
            if active.interrupt_requested {
                return Ok(false);
            }
            (active.id, active.pending_ask_user_question.clone())
        };
        let Some(pending) = pending else {
            return Ok(false);
        };
        if pending.tool_call_id != tool_call_id {
            self.emit_error(&format!(
                "AskUserQuestion response targeted stale tool_call_id {tool_call_id}; pending tool_call_id is {}.",
                pending.tool_call_id
            ));
            return Ok(true);
        }

        let updated_input = ask_user_question_input_with_answer(&pending.input, &message);
        let payload =
            ask_user_question_control_response_payload(&pending.request_id, updated_input.clone());
        if let Err(err) = self.write_process_json_line(&payload).await {
            let error = format!("Failed to send AskUserQuestion answer to Claude: {err}");
            self.fail_pending_ask_user_question(turn_id, &error).await;
            let outcome_tx = self.take_active_turn_outcome_sender(turn_id).await;
            self.retire_process_for_replacement().await;
            if let Some(outcome_tx) = outcome_tx {
                let _ = outcome_tx.send(TurnOutcome::Failed {
                    summary: ClaudeStdoutSummary::default(),
                    error: error.clone(),
                });
            }
            return Err(error);
        }

        let Some(_) = self
            .take_pending_ask_user_question(turn_id, &pending.request_id)
            .await
        else {
            return Ok(true);
        };

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
            if active.interrupt_requested {
                return Ok(false);
            }
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
            return Err(format!(
                "Failed to send ExitPlanMode response to Claude: {err}"
            ));
        }

        let Some(_) = self
            .take_pending_exit_plan_mode(turn_id, &pending.request_id)
            .await
        else {
            return Ok(true);
        };

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

        let (config, process_generation) = {
            let mut state = self.state.lock().await;
            // A turn reserved just before shutdown must not respawn the CLI
            // process after `shutdown_process` has killed it.
            if state.closing {
                return Err("Claude backend is shutting down".to_string());
            }
            state.process_generation = state.process_generation.saturating_add(1);
            let process_generation = state.process_generation;
            state.compact_command_advertised = None;
            state.provider_version = state.installed_provider_version.clone();
            state.compaction_capability = BackendCompactionCapability::unknown(
                BackendCompactionUnknownReason::ProcessNotInitialized,
                None,
                BackendCompactionCapabilityEvidence::None,
            );
            let config = ClaudeProcessSpawnConfig {
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
                // Rebuilt from session state on every process start, so a
                // respawn after a crash or a post-turn restart points at the
                // root this session already materialized.
                skill_plugin_root: state
                    .skill_plugin
                    .as_ref()
                    .map(|plugin| plugin.root().to_string_lossy().into_owned()),
            };
            let requires_resume_bootstrap = config.fork_from_session_id.is_some()
                || (config.resume_existing_session
                    && config.session_id.is_some()
                    && state.resume_bootstrap_required);
            if requires_resume_bootstrap {
                state.resume_bootstrap = Some(ClaudeResumeBootstrap {
                    generation: process_generation,
                    fork_session: config.fork_from_session_id.is_some(),
                    completions: Vec::new(),
                    quarantined_frames: 0,
                });
                tracing::info!(
                    process_generation,
                    fork = config.fork_from_session_id.is_some(),
                    "quarantining Claude CLI resume bootstrap until its terminal result"
                );
            } else {
                state.resume_bootstrap = None;
            }
            (config, process_generation)
        };

        // Re-armed per process: each process emits its own `init` frame, and a
        // respawn must be checked against its own rather than inheriting the
        // previous one's verdict.
        self.reset_skill_readiness().await;
        let startup_mcp_names = config
            .startup_mcp_config_json
            .as_deref()
            .and_then(|config| serde_json::from_str::<Value>(config).ok())
            .and_then(|config| config.get("mcpServers").and_then(Value::as_object).cloned())
            .map(|servers| {
                servers
                    .into_iter()
                    .map(|(name, _)| name)
                    .collect::<HashSet<_>>()
            })
            .unwrap_or_default();
        let runtime = match self.spawn_process(config, process_generation).await {
            Ok(runtime) => runtime,
            Err(error) => {
                self.fail_resume_bootstrap(process_generation, &error).await;
                return Err(error);
            }
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
        match tokio::time::timeout(
            CLAUDE_INITIALIZE_TIMEOUT,
            self.send_control_request_with_timeout("initialize", CLAUDE_INITIALIZE_TIMEOUT),
        )
        .await
        {
            Ok(Ok(response)) => {
                self.configure_capacity_from_initialize(&response).await;
                self.schedule_capacity_refresh().await;
                if !startup_mcp_names.is_empty()
                    && let Err(error) = self.await_startup_mcp_ready(&startup_mcp_names).await
                {
                    self.shutdown_process().await;
                    return Err(error);
                }
            }
            Ok(Err(err)) => {
                self.shutdown_process().await;
                return Err(err);
            }
            Err(_) => {
                self.shutdown_process().await;
                return Err("Timed out initializing Claude CLI control protocol".to_string());
            }
        }

        Ok(())
    }

    async fn await_startup_mcp_ready(
        &self,
        expected_names: &HashSet<String>,
    ) -> Result<(), String> {
        let required_names = expected_names
            .iter()
            .filter(|name| claude_startup_mcp_is_required(name))
            .cloned()
            .collect::<HashSet<_>>();
        let deadline = tokio::time::Instant::now() + CLAUDE_INITIALIZE_TIMEOUT;
        loop {
            let response = self
                .send_control_request_with_timeout("mcp_status", CLAUDE_INITIALIZE_TIMEOUT)
                .await?;
            let servers = response
                .get("mcpServers")
                .and_then(Value::as_array)
                .ok_or_else(|| "Claude MCP status omitted mcpServers".to_owned())?;
            let observed = servers
                .iter()
                .map(|server| {
                    (
                        server
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("<missing>"),
                        server
                            .get("status")
                            .and_then(Value::as_str)
                            .unwrap_or("<missing>"),
                    )
                })
                .collect::<Vec<_>>();
            tracing::info!(
                expected_names = ?expected_names,
                required_names = ?required_names,
                observed = ?observed,
                "Claude startup MCP readiness observation"
            );
            let configured = servers
                .iter()
                .filter(|server| {
                    server
                        .get("name")
                        .and_then(Value::as_str)
                        .is_some_and(|name| expected_names.contains(name))
                })
                .collect::<Vec<_>>();
            let required = configured
                .iter()
                .copied()
                .filter(|server| {
                    server
                        .get("name")
                        .and_then(Value::as_str)
                        .is_some_and(|name| required_names.contains(name))
                })
                .collect::<Vec<_>>();
            if required.len() == required_names.len()
                && required.iter().all(|server| {
                    server
                        .get("status")
                        .and_then(Value::as_str)
                        .is_some_and(|status| status.eq_ignore_ascii_case("connected"))
                })
            {
                for server in configured {
                    let Some(name) = server.get("name").and_then(Value::as_str) else {
                        continue;
                    };
                    let status = server
                        .get("status")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown");
                    if !required_names.contains(name) && !status.eq_ignore_ascii_case("connected") {
                        self.emitter.warning_message(&format!(
                            "Claude custom MCP server '{name}' is {status}; continuing without it"
                        ));
                    }
                }
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(format!(
                    "Timed out waiting for Claude MCP servers to connect: {response}"
                ));
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    async fn quarantine_resume_bootstrap_frame(
        &self,
        process_generation: u64,
        value: &Value,
    ) -> bool {
        let terminal = value.get("type").and_then(Value::as_str) == Some("result");
        let init = value.get("type").and_then(Value::as_str) == Some("system")
            && value.get("subtype").and_then(Value::as_str) == Some("init");
        let completions = {
            let mut state = self.state.lock().await;
            let Some(bootstrap) = state.resume_bootstrap.as_mut() else {
                let startup_result =
                    state.resume_empty_result_generation == Some(process_generation) && terminal;
                let ignore_empty = startup_result
                    && value
                        .get("result")
                        .and_then(Value::as_str)
                        .is_none_or(|result| result.trim().is_empty());
                if startup_result {
                    state.resume_empty_result_generation = None;
                }
                return ignore_empty;
            };
            if bootstrap.generation != process_generation {
                return false;
            }
            bootstrap.quarantined_frames = bootstrap.quarantined_frames.saturating_add(1);
            if init {
                let bootstrap = state
                    .resume_bootstrap
                    .take()
                    .expect("matching Claude resume bootstrap disappeared");
                if bootstrap.fork_session {
                    state.resume_empty_result_generation = Some(process_generation);
                }
                Some((bootstrap.completions, bootstrap.quarantined_frames))
            } else {
                None
            }
        };
        if let Some((completions, quarantined_frames)) = completions {
            tracing::info!(
                process_generation,
                quarantined_frames,
                "Claude CLI resume bootstrap reached the next turn boundary"
            );
            for completion in completions {
                let _ = completion.send(Ok(()));
            }
            false
        } else {
            tracing::debug!(
                process_generation,
                frame_type = value
                    .get("type")
                    .and_then(|value| value.as_str())
                    .unwrap_or(""),
                frame_subtype = value
                    .get("subtype")
                    .and_then(|value| value.as_str())
                    .unwrap_or(""),
                "quarantined Claude CLI resume bootstrap frame"
            );
            true
        }
    }

    async fn fail_resume_bootstrap(&self, process_generation: u64, error: &str) {
        let completions = {
            let mut state = self.state.lock().await;
            if state
                .resume_bootstrap
                .as_ref()
                .is_some_and(|bootstrap| bootstrap.generation == process_generation)
            {
                state
                    .resume_bootstrap
                    .take()
                    .map(|bootstrap| bootstrap.completions)
            } else {
                None
            }
        };
        if let Some(completions) = completions {
            tracing::warn!(process_generation, error, "Claude resume bootstrap failed");
            for completion in completions {
                let _ = completion.send(Err(error.to_string()));
            }
        }
    }

    async fn configure_capacity_from_initialize(&self, response: &Value) {
        let access = claude_capacity_access_from_initialize(response);
        let emitter = {
            let mut state = self.state.lock().await;
            state.capacity_access = access;
            state.compact_command_advertised = response
                .get("commands")
                .and_then(Value::as_array)
                .map(|commands| {
                    commands.iter().any(|command| {
                        command
                            .as_str()
                            .or_else(|| command.get("name").and_then(Value::as_str))
                            .is_some_and(|name| name.trim_start_matches('/') == "compact")
                    })
                });
            if state.provider_version.is_none() {
                state.provider_version = response
                    .get("claudeCodeVersion")
                    .or_else(|| response.get("version"))
                    .and_then(Value::as_str)
                    .and_then(normalize_nonempty);
            }
            if state.provider_version.is_some() {
                state.installed_provider_version = state.provider_version.clone();
            }
            state.compaction_capability = claude_compaction_capability(
                state.compact_command_advertised,
                state.provider_version.as_deref(),
            );
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

    async fn observe_process_metadata(&self, value: &Value) {
        if value.get("type").and_then(Value::as_str) != Some("system")
            || value.get("subtype").and_then(Value::as_str) != Some("init")
        {
            return;
        }
        let version = value
            .get("claude_code_version")
            .or_else(|| value.get("version"))
            .and_then(Value::as_str)
            .and_then(normalize_nonempty);
        if version.is_none() {
            return;
        }
        let mut state = self.state.lock().await;
        state.installed_provider_version = version.clone();
        state.provider_version = version;
        state.compaction_capability = claude_compaction_capability(
            state.compact_command_advertised,
            state.provider_version.as_deref(),
        );
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
        process_generation: u64,
    ) -> Result<ClaudeProcessRuntime, String> {
        let cli_args = build_claude_cli_args(&config);
        let mut child = if let Some(host) = config.ssh_host.as_deref() {
            let pinned_model = config.model.as_deref().and_then(normalize_nonempty);
            let model_env = pinned_model.as_deref().map_or_else(Vec::new, |model| {
                vec![
                    ("ANTHROPIC_MODEL", model),
                    ("CLAUDE_CODE_AUTO_MODE_MODEL", model),
                    ("CLAUDE_CODE_BG_CLASSIFIER_MODEL", model),
                    ("CLAUDE_CODE_SUBAGENT_MODEL", model),
                    ("CLAUDE_CONTEXT_COLLAPSE_MODEL", model),
                    ("CLAUDE_CODE_NO_MODEL_FALLBACK", "1"),
                ]
            });
            crate::remote::spawn_remote_process_with_env(
                host,
                "claude",
                &cli_args,
                Some(&config.workspace_root),
                &model_env,
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
            if let Some(model) = config.model.as_deref().and_then(normalize_nonempty) {
                cmd.env("ANTHROPIC_MODEL", &model);
                cmd.env("CLAUDE_CODE_AUTO_MODE_MODEL", &model);
                cmd.env("CLAUDE_CODE_BG_CLASSIFIER_MODEL", &model);
                cmd.env("CLAUDE_CODE_SUBAGENT_MODEL", &model);
                cmd.env("CLAUDE_CONTEXT_COLLAPSE_MODEL", &model);
                cmd.env("CLAUDE_CODE_NO_MODEL_FALLBACK", "1");
                eprintln!("TYDE CLAUDE EXPLICIT MODEL PIN model={model}");
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
        self.activate_background_task_owner();
        let stdout_task = tokio::spawn(read_claude_stdout_persistent(
            stdout,
            Arc::clone(self),
            Arc::clone(&control_waiters),
            Arc::clone(&stdin),
            process_generation,
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
            if active.outcome_tx.is_some() || matches!(active.owner, ClaudeTurnOwner::Compaction(_))
            {
                Some(active.id)
            } else {
                None
            }
        })
    }

    async fn complete_active_turn_with_outcome(&self, turn_id: u64, outcome: TurnOutcome) -> bool {
        let tx = self.take_active_turn_outcome_sender(turn_id).await;
        if let Some(tx) = tx {
            let _ = tx.send(outcome);
            true
        } else {
            false
        }
    }

    async fn take_active_turn_outcome_sender(
        &self,
        turn_id: u64,
    ) -> Option<oneshot::Sender<TurnOutcome>> {
        {
            let mut state = self.state.lock().await;
            let active = state.active_turn.as_mut()?;
            if active.id != turn_id {
                return None;
            }
            active.outcome_tx.take()
        }
    }

    /// The identity to decide a skill gap against, taken now.
    async fn current_skill_failure_target(&self) -> SkillFailureTarget {
        let state = self.state.lock().await;
        SkillFailureTarget {
            generation: state.skill_verification_generation,
            turn_id: state.active_turn.as_ref().map(|active| active.id),
        }
    }

    /// Commit a skill gap, or decline it.
    ///
    /// Every check and every mutation happens in one critical section under the
    /// state lock — the same lock a cancel takes. That makes the two linearize:
    /// whichever acquires it first wins, and the loser's revalidation fails
    /// rather than half-applying. A gap declines when the process has moved on,
    /// when verification is no longer pending, when a cancel already marked
    /// abandonment, when the active turn is not the exact one the gap was
    /// decided for, or when that turn has been interrupted.
    ///
    /// Declining emits nothing: a cancelled turn must not be reported as a
    /// missing skill, and a gap decided against a turn that is no longer running
    /// would name the wrong one.
    async fn commit_skill_failure(
        &self,
        target: &SkillFailureTarget,
        reason: &str,
    ) -> Option<CommittedSkillFailure> {
        let mut state = self.state.lock().await;

        if state.skill_verification_generation != target.generation {
            tracing::debug!(
                "Declining a Claude skill failure from generation {}; the session is on {}",
                target.generation,
                state.skill_verification_generation
            );
            return None;
        }
        if self.skill_verification_abandoned.load(Ordering::Relaxed) {
            tracing::debug!("Declining a Claude skill failure; the turn was cancelled first");
            return None;
        }
        if !matches!(
            *self.skill_readiness.borrow(),
            ClaudeSkillReadiness::Pending
        ) {
            tracing::debug!("Declining a Claude skill failure; verification already settled");
            return None;
        }

        let active_turn_id = state.active_turn.as_ref().map(|active| active.id);
        if active_turn_id != target.turn_id {
            tracing::debug!(
                "Declining a Claude skill failure decided for turn {:?}; turn {:?} is active",
                target.turn_id,
                active_turn_id
            );
            return None;
        }
        if state
            .active_turn
            .as_ref()
            .is_some_and(|active| active.interrupt_requested)
        {
            tracing::debug!("Declining a Claude skill failure; that turn was interrupted");
            return None;
        }

        // Committed. Transition and disown the watchdog. The turn's outcome
        // sender is deliberately left alone: the turn is still the user's turn
        // and still completes on its own terms. A skill Tyde could not expose
        // does not make the model's answer void.
        self.skill_readiness
            .send_replace(ClaudeSkillReadiness::Degraded(reason.to_string()));
        let watchdog = state.skill_watchdog.take();
        Some(CommittedSkillFailure { watchdog })
    }

    /// Report a skill gap decided against `target`, and let the session run on.
    ///
    /// The `init` frame arrives at the head of the CLI's response to the first
    /// user message, so by the time this runs the first provider request is
    /// already in flight — that race is unavoidable without a pre-start skill
    /// inventory the CLI does not expose (see `verify_plugin_inventory`). An
    /// earlier revision resolved it by suppressing the model's output, failing
    /// the turn and killing the process. That traded one missing skill for the
    /// whole session, and did it most often for the most harmless cause: Claude
    /// dropping a plugin skill whose name collided with one the user already
    /// had, where the capability is in fact still there under its own name.
    ///
    /// So the gap is now a notice. It is emitted once, it names the skill, and
    /// the session keeps every other skill, its turn, and its process.
    ///
    /// `abort_watchdog` is false when the caller *is* the watchdog: aborting its
    /// own handle would cancel it at the next await, which is how an earlier
    /// revision lost the settle.
    async fn report_skill_gap(
        &self,
        target: &SkillFailureTarget,
        message: &str,
        abort_watchdog: bool,
    ) {
        let Some(committed) = self.commit_skill_failure(target, message).await else {
            return;
        };
        tracing::warn!("{message}");
        // A notice, not a `backend_error`: an error card reads as "this turn
        // failed", and this turn did not.
        self.emitter.subprocess_stderr(message);
        if let Some(handle) = committed.watchdog
            && abort_watchdog
        {
            handle.abort();
        }
    }

    /// Record the CLI's `init` frame against what Tyde materialized.
    ///
    /// The frame is produced locally before any provider request, so this costs
    /// nothing. A missing skill means the CLI dropped it — most likely a
    /// collision with a user-owned skill of the same name, which the CLI
    /// resolves in the user's favour and logs only at debug level. Tyde says so
    /// out loud, because the alternative is a session that believes it has a
    /// skill it does not have.
    async fn record_skill_init_frame(&self, reported: Result<Option<Vec<String>>, String>) {
        let expected = {
            let state = self.state.lock().await;
            if state.expected_skills.is_empty() {
                return;
            }
            state.expected_skills.clone()
        };
        if *self.skill_readiness.borrow() != ClaudeSkillReadiness::Pending {
            // Already settled for this process; a later frame does not re-open it.
            return;
        }
        let verdict = match reported {
            Ok(reported) => verify_init_frame(&expected, reported.as_deref()),
            Err(message) => InitFrameVerdict::Degraded(message),
        };
        match verdict {
            InitFrameVerdict::Verified => {
                tracing::debug!(
                    "Claude reported all {} Tyde skill(s) in its init frame",
                    expected.len()
                );
                self.skill_readiness
                    .send_replace(ClaudeSkillReadiness::Ready);
                self.cancel_skill_watchdog().await;
            }
            InitFrameVerdict::Degraded(message) => {
                // The transition is left to `commit_skill_failure`. Setting it
                // here would both pre-empt the atomic check and guarantee the
                // commit declines, since it requires readiness to still be
                // `Pending` — the very condition a cancel races it for.
                let target = self.current_skill_failure_target().await;
                self.report_skill_gap(&target, &message, true).await;
            }
        }
    }

    /// Is this session still waiting to learn whether it has its skills?
    ///
    /// While true, the stdout reader holds model output back so the notice for
    /// a skill that did not arrive lands ahead of the output it is about.
    fn skills_awaiting_verification(&self) -> bool {
        if !matches!(
            *self.skill_readiness.borrow(),
            ClaudeSkillReadiness::Pending
        ) {
            return false;
        }
        // A cancelled turn leaves readiness pending but nothing to act on: no
        // hold-back, no terminal failure, no watchdog. Checked here rather than
        // at each call site so no pending path can be added later that forgets.
        !self.skill_verification_abandoned.load(Ordering::Relaxed)
    }

    /// Bound the wait for an `init` frame once a prompt has gone out.
    ///
    /// The CLI reports its skills at the head of its response, so a prompt that
    /// produces no `init` frame at all — an older CLI, a changed frame shape —
    /// would otherwise leave the session holding its output forever. This turns
    /// that into a visible failure on the same timeout the handshake uses.
    async fn watch_for_skill_verification(self: &Arc<Self>) {
        // Everything under one lock, in the caller's task. The previous version
        // spawned a task to read the generation and register the handle, which
        // left a window: a cancel landing inside it found no handle to abort
        // and then the spawned task read the *post-cancel* generation, so both
        // guards passed and the timer fired on a cancelled turn.
        let mut state = self.state.lock().await;
        if !matches!(
            *self.skill_readiness.borrow(),
            ClaudeSkillReadiness::Pending
        ) || self.skill_verification_abandoned.load(Ordering::Relaxed)
        {
            return;
        }
        // The identity this timer is for, captured now under the same lock that
        // arms it. Revalidated by `commit_skill_failure` when it fires, so a
        // timer that outlives its process or its turn declines instead of
        // settling something it was never about.
        let target = SkillFailureTarget {
            generation: state.skill_verification_generation,
            turn_id: state.active_turn.as_ref().map(|active| active.id),
        };
        let inner = Arc::clone(self);
        let handle = tokio::spawn(async move {
            tokio::time::sleep(CLAUDE_SKILL_VERIFICATION_TIMEOUT).await;
            // `abort_watchdog: false` — this task owns the handle it would be
            // aborting, and cancelling itself would drop the settle.
            inner
                .report_skill_gap(
                    &target,
                    "Claude did not report which skills it loaded within the startup timeout.",
                    false,
                )
                .await;
        });
        if let Some(previous) = state.skill_watchdog.replace(handle) {
            previous.abort();
        }
    }

    /// Stop the verification watchdog, if one is armed.
    async fn cancel_skill_watchdog(&self) {
        let handle = self.state.lock().await.skill_watchdog.take();
        if let Some(handle) = handle {
            handle.abort();
        }
    }

    /// Forget a pending verification because the turn was cancelled.
    ///
    /// A cancelled turn is not a skill failure, and reporting one would blame
    /// the skills for something the user did. The watchdog is stopped, the
    /// generation is bumped so any in-flight timer is inert, and readiness is
    /// left `Pending` for whichever process starts next.
    /// Abandon verification because *this* turn was cancelled.
    ///
    /// Only ever acts when there is an active turn that has actually been
    /// interrupted. Marking abandonment with no such turn would suppress the
    /// terminal paths for a session nobody cancelled — a silent way to lose a
    /// real skill failure. Returns whether it marked, so the caller can retire
    /// the process that will now never report.
    async fn abandon_skill_verification_for_cancelled_turn(&self) -> bool {
        let matching = {
            let state = self.state.lock().await;
            state
                .active_turn
                .as_ref()
                .is_some_and(|active| active.interrupt_requested)
        };
        if !matching {
            return false;
        }
        if !self.skills_awaiting_verification() {
            // Nothing pending to abandon; a cancel does not need to suppress
            // anything, and marking would outlive this turn for no reason.
            return false;
        }
        // Mark first, so a watchdog firing between the mark and the abort sees
        // it and returns instead of failing the session.
        self.skill_verification_abandoned
            .store(true, Ordering::Relaxed);
        {
            let mut state = self.state.lock().await;
            state.skill_verification_generation =
                state.skill_verification_generation.wrapping_add(1);
        }
        self.cancel_skill_watchdog().await;
        true
    }

    /// Was the turn currently in flight interrupted by the user?
    async fn active_turn_is_interrupted(&self) -> bool {
        self.state
            .lock()
            .await
            .active_turn
            .as_ref()
            .is_some_and(|active| active.interrupt_requested)
    }

    /// Arm skill verification for a session that materialized `expected`.
    ///
    /// This is the **only** place a session's expected set and its initial
    /// readiness are established, so a test that arms a session takes exactly
    /// the path production does. Setting the two independently — a struct field
    /// here, a channel value there — is what let production and the tests drift
    /// into different states.
    async fn arm_skill_verification(&self, expected: Vec<String>) {
        self.skill_verification_abandoned
            .store(false, Ordering::Relaxed);
        let required = {
            let mut state = self.state.lock().await;
            state.expected_skills = expected;
            !state.expected_skills.is_empty()
        };
        self.skill_readiness.send_replace(if required {
            ClaudeSkillReadiness::Pending
        } else {
            ClaudeSkillReadiness::NotRequired
        });
    }

    /// Re-arm verification for a process that is about to start.
    async fn reset_skill_readiness(&self) {
        // A new process may report, so a cancel on the previous one stops
        // suppressing.
        self.skill_verification_abandoned
            .store(false, Ordering::Relaxed);
        self.cancel_skill_watchdog().await;
        let required = {
            let mut state = self.state.lock().await;
            // A new process gets a new generation, so any watchdog still
            // sleeping for the old one can no longer act.
            state.skill_verification_generation =
                state.skill_verification_generation.wrapping_add(1);
            !state.expected_skills.is_empty()
        };
        if required {
            self.skill_readiness
                .send_replace(ClaudeSkillReadiness::Pending);
        }
    }

    async fn take_restart_process_after_turn(&self) -> bool {
        let mut state = self.state.lock().await;
        let restart = state.restart_process_after_turn;
        state.restart_process_after_turn = false;
        restart
    }

    async fn shutdown_process(&self) {
        let resume_bootstrap_generation = self
            .state
            .lock()
            .await
            .resume_bootstrap
            .as_ref()
            .map(|bootstrap| bootstrap.generation);
        if let Some(process_generation) = resume_bootstrap_generation {
            self.fail_resume_bootstrap(
                process_generation,
                "Claude process shut down before its resume bootstrap reached a terminal result",
            )
            .await;
        }
        let runtime = self.runtime.lock().await.take();
        self.drain_background_tasks();
        if let Some(runtime) = runtime {
            runtime.kill().await;
        }
    }

    async fn shutdown_process_gracefully(&self) {
        let mut task_ids = self
            .background_tasks
            .lock()
            .expect("Claude background task mutex poisoned")
            .entries
            .iter()
            .filter(|(_, entry)| entry.state.status == BackgroundTaskStatus::Running)
            .map(|(task_id, _)| task_id.clone())
            .collect::<Vec<_>>();
        task_ids.extend(
            self.native_subagent_tasks
                .lock()
                .expect("Claude native subagent task mutex poisoned")
                .iter()
                .cloned(),
        );
        task_ids.sort();
        task_ids.dedup();
        eprintln!("TYDE CLAUDE CLEANUP native_and_background_tasks={task_ids:?}");
        for task_id in task_ids {
            let request_id = uuid::Uuid::new_v4().to_string();
            let request = json!({
                "type": "control_request",
                "request_id": request_id,
                "request": { "subtype": "stop_task", "task_id": task_id },
            });
            match self
                .send_control_request_value(request_id, request, CLAUDE_CONTROL_RESPONSE_TIMEOUT)
                .await
            {
                Ok(response) => {
                    eprintln!("TYDE CLAUDE CLEANUP stop_task task_id={task_id} response={response}")
                }
                Err(err) => tracing::warn!(task_id, "Failed to stop Claude background task: {err}"),
            }
        }
        let runtime = self.runtime.lock().await.take();
        if let Some(runtime) = runtime {
            runtime.shutdown().await;
        }
        self.drain_background_tasks();
        self.native_subagent_tasks
            .lock()
            .expect("Claude native subagent task mutex poisoned")
            .clear();
    }

    async fn retire_process_for_replacement(&self) {
        let runtime = self.runtime.lock().await.take();
        self.drain_background_tasks();
        if let Some(runtime) = runtime {
            runtime.abort_readers();
            tokio::spawn(runtime.kill());
        }
    }

    async fn mark_process_exited(&self) {
        self.drain_background_tasks();
        let runtime = self.runtime.lock().await.take();
        if let Some(runtime) = runtime {
            let mut child = runtime.child.lock().await;
            if let Some(child) = child.as_mut() {
                let _ = child.try_wait();
            }
        }
    }

    fn drain_background_tasks(&self) {
        let mut registry = self
            .background_tasks
            .lock()
            .expect("Claude background task mutex poisoned");
        registry.owner_active = false;
        drain_background_task_entries(&mut registry.entries);
        self.background_work_active
            .store(false, std::sync::atomic::Ordering::Relaxed);
        self.pending_cli_wake
            .store(false, std::sync::atomic::Ordering::Relaxed);
    }

    fn activate_background_task_owner(&self) {
        let mut registry = self
            .background_tasks
            .lock()
            .expect("Claude background task mutex poisoned");
        registry.owner_active = true;
    }

    fn handle_background_task_frame(
        &self,
        value: &Value,
        subagent_streams: &HashMap<String, SubAgentStream>,
    ) -> bool {
        let mut registry = self
            .background_tasks
            .lock()
            .expect("Claude background task mutex poisoned");
        if !registry.owner_active {
            if value.get("type").and_then(Value::as_str) == Some("system")
                && value.get("subtype").and_then(Value::as_str) == Some("task_started")
                && value.get("task_type").and_then(Value::as_str) == Some("local_bash")
            {
                tracing::warn!(
                    task_id = value
                        .get("task_id")
                        .and_then(|value| value.as_str())
                        .unwrap_or(""),
                    "ignoring background Bash start after Claude process owner loss"
                );
                return true;
            }
            return false;
        }
        handle_background_bash_task_frame_with_owners(
            value,
            &mut registry.entries,
            &self.emitter,
            subagent_streams,
        )
    }

    async fn cancel_active_turn(&self) {
        let (turn_id, quiesced_rx) = {
            // Serialize the state transition with interactive responses, but
            // release the event gate before awaiting provider quiescence: the
            // stdout result that signals quiescence must acquire this gate.
            let _turn_event_guard = self.turn_event_gate.lock().await;
            let mut state = self.state.lock().await;
            let foreground_already_quiescent = self
                .background_work_active
                .load(std::sync::atomic::Ordering::Relaxed)
                && state
                    .active_turn
                    .as_ref()
                    .is_some_and(|active| active.outcome_tx.is_none());
            if foreground_already_quiescent {
                drop(state);
                tracing::info!(
                    "Claude interrupt observed a quiescent foreground turn while background work continues"
                );
                self.emit_operation_cancelled(
                    "Claude foreground turn already ended; background work continues.",
                );
                return;
            }
            let Some(active) = state.active_turn.as_mut() else {
                let message = if self
                    .background_work_active
                    .load(std::sync::atomic::Ordering::Relaxed)
                {
                    "Claude foreground turn already ended; background work continues."
                } else {
                    "No Claude foreground turn was running."
                };
                drop(state);
                tracing::info!(
                    background_work_active = self
                        .background_work_active
                        .load(std::sync::atomic::Ordering::Relaxed),
                    "Claude interrupt found no foreground turn"
                );
                self.emit_operation_cancelled(message);
                return;
            };
            let (quiesced_tx, quiesced_rx) = oneshot::channel();
            active.quiesced_waiters.push(quiesced_tx);
            active.interrupt_requested = true;
            tracing::info!(
                turn_id = active.id,
                "Claude interrupt targeted foreground turn"
            );
            let target = (active.id, quiesced_rx);
            drop(state);
            // Keep terminalizing pending interactions in the same critical
            // section that marks the turn interrupted, so no answer can be
            // written to Claude between those two lifecycle transitions.
            self.emitter
                .cancel_pending_foreground_tools("Cancelled by user");
            target
        };

        // Only now, with the turn confirmed interrupted, may verification be
        // abandoned — a cancelled turn is the user's doing, not the skills'.
        let abandoned_verification = self.abandon_skill_verification_for_cancelled_turn().await;

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
                    // Bounded. This waiter is only ever signalled by
                    // `clear_active_turn`, which runs from `run_turn` and
                    // `finalize_turn`. If the owning turn task is gone — the
                    // process died, the task was aborted — nothing will call
                    // it, and an unbounded wait here wedges shutdown forever.
                    if tokio::time::timeout(CLAUDE_INTERRUPT_QUIESCE_TIMEOUT, rx)
                        .await
                        .is_err()
                    {
                        tracing::warn!(
                            "No owner left to quiesce Claude turn {turn_id}; clearing it here"
                        );
                        for waiter in self.clear_active_turn(turn_id).await {
                            let _ = waiter.send(());
                        }
                    }
                }
            }
        }

        // A process whose pending verification was abandoned can never report
        // its skills: the init frame only follows the first user message, and
        // that turn is now cancelled. Retire it so the next turn spawns a fresh
        // process, which re-arms with a new generation rather than inheriting a
        // session that is permanently unverifiable.
        if abandoned_verification {
            tracing::debug!(
                "Retiring the Claude process for cancelled turn {turn_id}; its verification \
                 was abandoned and it can no longer report"
            );
            self.shutdown_process().await;
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
                state.resume_bootstrap_required = true;
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
                state.resume_bootstrap_required = true;
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
        self.typing_active
            .store(false, std::sync::atomic::Ordering::Relaxed);
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
        let resume_bootstrap_required = claude_replay_requires_resume_bootstrap(&replay.items);
        for item in replay.items {
            match item {
                ClaudeHistoryReplayItem::Message(message) => {
                    self.emit_replay_message(message);
                }
                ClaudeHistoryReplayItem::ToolRequest(tool_call) => {
                    self.emit_replay_tool_request(&tool_call);
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
                ClaudeHistoryReplayItem::Compaction(observation) => {
                    self.emitter
                        .compaction_event(&BackendCompactionEvent::Observed(Box::new(observation)));
                }
            }
        }

        let mut state = self.state.lock().await;
        state.cumulative_usage = replay.cumulative_usage;
        state.cumulative_usage_complete = replay.cumulative_usage_complete;
        state.conversation_bytes_total = replay.conversation_bytes_total;
        state.start_session_fresh = false;
        state.resume_bootstrap_required = resume_bootstrap_required;
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

    fn emit_tool_request(&self, tool_call: &ClaudeToolCall) -> bool {
        self.emit_tool_request_with_ownership(tool_call, true)
    }

    fn emit_replay_tool_request(&self, tool_call: &ClaudeToolCall) {
        let _ = self.emit_tool_request_with_ownership(tool_call, false);
    }

    fn emit_tool_request_with_ownership(
        &self,
        tool_call: &ClaudeToolCall,
        require_declared_response: bool,
    ) -> bool {
        let task_update = self
            .task_tracker
            .lock()
            .expect("Claude task tracker mutex poisoned")
            .observe_request(tool_call);
        if let Some(tasks) = task_update {
            self.emitter.task_update(&tasks);
        }
        let tool_type = claude_tool_request_type(&tool_call.name, &tool_call.arguments);
        let tool_type = serde_json::from_value(tool_type.clone())
            .unwrap_or(ToolRequestType::Other { args: tool_type });
        let emitted = self.emitter.tool_request(&tool_call.id, tool_type);
        if !require_declared_response && !emitted {
            return false;
        }
        if !emitted {
            return false;
        }
        if is_subagent_tool_name(&tool_call.name) {
            let inserted = self
                .native_subagent_tasks
                .lock()
                .expect("Claude native subagent task mutex poisoned")
                .insert(tool_call.id.clone());
            eprintln!(
                "TYDE CLAUDE NATIVE TASK TRACK request={} inserted={inserted}",
                tool_call.id
            );
        }
        self.adopt_background_task_awaiting_tool_request(&tool_call.id);
        true
    }

    /// Claim a background task whose `task_started` frame arrived before Tyde
    /// had a tool request to identify it with.
    ///
    /// Ownership is normally resolved at `task_started` by asking the emitter
    /// for the launching tool request. Tyde only registers that request when
    /// the response phase closes (`message_stop`, or the close at the top of
    /// `consume_user_tool_result`), and the CLI does not guarantee the task
    /// frame arrives after it: once another background task is running, a
    /// captured 2.1.220 stream puts `task_started` — and the `tool_result` —
    /// ahead of `message_delta`/`message_stop`. An entry that misses its owner
    /// stays unresolved, which costs the tray its row *and* drops the task's
    /// terminal frame. Resolving here removes the ordering dependency
    /// entirely: whenever the request finally lands, the task adopts it.
    fn adopt_background_task_awaiting_tool_request(&self, tool_call_id: &str) {
        let mut registry = self
            .background_tasks
            .lock()
            .expect("Claude background task mutex poisoned");
        let Some(task_id) = registry.entries.iter().find_map(|(task_id, entry)| {
            (entry.tool_use_id == tool_call_id).then(|| task_id.clone())
        }) else {
            return;
        };
        let entry = registry
            .entries
            .get_mut(&task_id)
            .expect("background task disappeared while registry was locked");
        if entry.owner.is_some() && entry.tool_name.is_some() {
            return;
        }
        // Only the root stream reaches this path; a task owned by a sub-agent
        // resolves through that sub-agent's own stream and must not be
        // adopted here.
        if entry.parent_tool_use_id.is_some() {
            return;
        }
        let owner = Arc::clone(&self.emitter);
        entry.owner.get_or_insert_with(|| Arc::clone(&owner));
        if entry.tool_name.is_none() {
            entry.tool_name = owner.tool_request_name(tool_call_id);
        }
        if entry.state.description.is_none() {
            entry.state.description = owner.tool_request_command(tool_call_id);
        }
        let mut completion_emitted = false;
        if entry.tool_name.is_some() {
            tracing::debug!(
                task_id = entry.state.task_id,
                tool_use_id = tool_call_id,
                "adopted background task on its late tool request"
            );
            if entry.state.status == BackgroundTaskStatus::Running {
                emit_background_task_snapshot(&owner, entry);
            } else if entry.terminal_notification_received {
                emit_background_task_completion(&owner, entry);
                completion_emitted = true;
            }
        }
        if completion_emitted {
            registry.entries.remove(&task_id);
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
        let task_update = if success {
            self.task_tracker
                .lock()
                .expect("Claude task tracker mutex poisoned")
                .observe_completion(tool_call_id, tool_name, &tool_result)
        } else {
            None
        };
        let tool_result = claude_public_tool_result(tool_name, success, tool_result);
        let error = if is_subagent_tool_name(tool_name) && !success {
            Some("Agent task failed".to_owned())
        } else {
            error
        };
        let outcome = claude_tool_execution_outcome(success, tool_result, error);
        let pending_before_completion = self.emitter.has_pending_tool_request(tool_call_id);
        eprintln!(
            "TYDE CLAUDE TOOL COMPLETION id={tool_call_id} name={tool_name} pending={pending_before_completion} known={}",
            self.emitter.has_known_tool_request(tool_call_id)
        );
        let emitted =
            emit_tool_completion_for_known_request(&self.emitter, tool_call_id, tool_name, outcome);
        tracing::info!(
            tool_call_id,
            tool_name,
            pending_before_completion,
            emitted,
            "Claude tool completion emission"
        );
        if let Some(tasks) = task_update {
            self.emitter.task_update(&tasks);
        }
    }

    async fn shutdown(&self) {
        tracing::info!("Claude shutdown starting");
        self.state.lock().await.closing = true;
        self.cancel_skill_watchdog().await;
        if self.state.lock().await.active_turn.is_some() {
            self.cancel_active_turn().await;
        }
        self.shutdown_process_gracefully().await;
        tracing::info!("Claude process shutdown completed");
        // Unlink the session plugin root once the CLI that was reading it is
        // gone. Dropping the last `Arc` would do this anyway, but a session can
        // outlive its shutdown in a caller's hands, and leaving a root in
        // TMPDIR until then is a leak the user can see.
        let plugin = self.state.lock().await.skill_plugin.take();
        if let Some(plugin) = plugin
            && let Err(err) = plugin.cleanup()
        {
            tracing::warn!("Failed to clean up Claude skill plugin: {err}");
        }
        self.emitter.close("Claude session closed");
        tracing::info!("Claude shutdown completed");
    }

    fn emit_typing_status(&self, typing: bool) {
        if self
            .typing_active
            .swap(typing, std::sync::atomic::Ordering::Relaxed)
            != typing
        {
            self.emitter.typing_status_changed(typing);
        }
    }

    fn emit_stream_start(&self, message_id: &str, model: Option<String>) {
        let response = self.emitter.stream_start(model.as_deref());
        *self
            .active_response
            .lock()
            .expect("Claude response mutex poisoned") = Some((message_id.to_owned(), response));
    }

    fn emit_user_message_added(&self, content: &str, images: Option<&[ImageAttachment]>) {
        let image_payload = images.map(|images| {
            images
                .iter()
                .map(|image| ImageData {
                    media_type: image.media_type.clone(),
                    data: image.data.clone(),
                })
                .collect::<Vec<_>>()
        });
        self.emitter.user_message(content, image_payload);
    }

    fn emit_stream_delta(&self, message_id: &str, text: &str) {
        let Some(response) = self.response_handle(message_id, "text delta") else {
            debug_assert!(
                false,
                "Claude emitted a text delta outside its active response"
            );
            return;
        };
        self.emitter.stream_delta(&response, text);
    }

    fn emit_stream_reasoning_delta(&self, message_id: &str, text: &str) {
        let Some(response) = self.response_handle(message_id, "reasoning delta") else {
            debug_assert!(
                false,
                "Claude emitted a reasoning delta outside its active response"
            );
            return;
        };
        self.emitter.stream_reasoning_delta(&response, text);
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
        let content_offset = u32::try_from(content.chars().count()).unwrap_or(u32::MAX);
        let Some(response) = self
            .active_response
            .lock()
            .expect("Claude response mutex poisoned")
            .take()
            .map(|(_, response)| response)
        else {
            tracing::error!("Claude ended a response that was not open");
            debug_assert!(false, "Claude ended a response that was not open");
            return;
        };
        self.emitter.stream_end(response, StreamEndPayload {
            content,
            model_info: model.map(|model| ModelInfo { model }),
            token_usage: claude_message_token_usage(usage),
            reasoning: reasoning.map(|text| ReasoningData {
                text,
                tokens: None,
                signature: None,
                blob: None,
            }),
            tool_calls: tool_calls
                .into_iter()
                .filter_map(|tool_call| claude_tool_use_data(tool_call, content_offset))
                .collect(),
            context_breakdown: context_breakdown.and_then(|value| {
                serde_json::from_value::<ContextBreakdown>(value)
                    .map_err(|error| tracing::warn!(%error, "dropping invalid Claude context breakdown"))
                    .ok()
            }),
            images: Vec::new(),
        });
    }

    fn response_handle(&self, message_id: &str, event: &str) -> Option<ResponseHandle> {
        let active = self
            .active_response
            .lock()
            .expect("Claude response mutex poisoned");
        let Some((active_message_id, response)) = active.as_ref() else {
            tracing::error!(
                message_id,
                event,
                "Claude response event arrived without StreamStart"
            );
            return None;
        };
        if active_message_id != message_id {
            tracing::error!(
                message_id,
                active_message_id,
                event,
                "Claude response event used a stale response key"
            );
            return None;
        }
        Some(response.clone())
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
        self.typing_active
            .store(false, std::sync::atomic::Ordering::Relaxed);
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
                .cloned()
                .and_then(|images| serde_json::from_value(images).ok());
            self.emitter.user_message(content, images);
            return;
        }

        // Anything non-User during replay is an assistant message.
        let content = message
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let reasoning = message
            .get("reasoning")
            .cloned()
            .filter(|value| !value.is_null())
            .and_then(|value| serde_json::from_value::<ReasoningData>(value).ok());
        let tool_calls = message
            .get("tool_calls")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let model_info = message
            .get("model_info")
            .cloned()
            .filter(|value| !value.is_null())
            .and_then(|value| serde_json::from_value::<ModelInfo>(value).ok());
        let token_usage = message
            .get("token_usage")
            .cloned()
            .filter(|value| !value.is_null())
            .and_then(|value| serde_json::from_value::<MessageTokenUsage>(value).ok());
        let context_breakdown = message
            .get("context_breakdown")
            .cloned()
            .filter(|value| !value.is_null())
            .and_then(|value| serde_json::from_value::<ContextBreakdown>(value).ok());
        let images = message
            .get("images")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|image| serde_json::from_value::<ImageData>(image.clone()).ok())
            .collect::<Vec<_>>();
        let content_offset = u32::try_from(content.chars().count()).unwrap_or(u32::MAX);
        let tool_calls = tool_calls
            .into_iter()
            .filter_map(|tool_call| claude_tool_use_data(tool_call, content_offset))
            .collect();
        self.emitter
            .replay_assistant_message(AssistantMessagePayload {
                message_id: None,
                content,
                reasoning,
                tool_calls,
                model_info,
                token_usage,
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
        options: ClaudeTerminalPhaseOptions,
    ) -> bool {
        let ClaudeTerminalPhaseOptions {
            turn_id,
            conversation_history_bytes,
            known_context_window,
            model_hint,
            turn_usage,
            cancelled,
        } = options;
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
            if !self.emitter.is_stream_open() {
                self.emit_stream_start(
                    &format!("claude-msg-{turn_id}-terminal"),
                    selected_model.clone(),
                );
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
            close_terminal_tool_requests(summary, self, cancelled);
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
                    ClaudeControlEvent::ConversationCompacted => {}
                }
                if !self.emitter.is_stream_open() {
                    self.emit_stream_start(
                        &format!("claude-msg-{turn_id}-terminal"),
                        selected_model.clone(),
                    );
                }
                self.emit_placeholder_stream_end(
                    selected_model,
                    turn_usage.clone(),
                    Some(context_breakdown),
                );
            }
            close_terminal_tool_requests(summary, self, cancelled);
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
            if !self.emitter.is_stream_open() {
                self.emit_stream_start(
                    &format!("claude-msg-{turn_id}-terminal"),
                    selected_model.clone(),
                );
            }
            self.emit_placeholder_stream_end(selected_model, turn_usage, Some(context_breakdown));
        }

        if !summary.unresolved_tool_requests.is_empty() {
            close_terminal_tool_requests(summary, self, cancelled);
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

/// Run `claude --plugin-dir <root> plugin list --json` and check the result.
///
/// This spends no provider tokens — it is a local inventory command — and runs
/// **before** the session process starts. `plugin list --json` is the only
/// stable machine-readable plugin surface the CLI exposes: `plugin details`
/// prints skills as unstructured text with no `--json`, and
/// `plugin validate --strict` prints "Validation failed" while exiting 0, so
/// neither can carry a gate. Per-skill verification therefore happens later,
/// against the `init` frame.
async fn claude_verify_plugin_loaded(root: &Path, workspace_root: &str) -> Result<(), String> {
    let mut cmd = Command::new(claude_binary());
    cmd.arg(CLAUDE_PLUGIN_DIR_FLAG)
        .arg(root)
        .arg("plugin")
        .arg("list")
        .arg("--json");
    // Same provenance as the session process, or the answer is about a
    // different configuration than the one that will run. Plugin resolution is
    // cwd-sensitive (project-scoped plugins) and config-sensitive
    // (`CLAUDE_CONFIG_DIR`), so the probe runs in the session's workspace and
    // inherits the same environment, with only `PATH` overridden the same way
    // `spawn_process` overrides it.
    cmd.current_dir(workspace_root);
    if let Some(path) = process_env::resolved_child_process_path() {
        cmd.env("PATH", path);
    }
    let output = tokio::time::timeout(CLAUDE_INITIALIZE_TIMEOUT, cmd.output())
        .await
        .map_err(|_| {
            "Timed out asking the Claude CLI which plugins it loaded; Tyde cannot confirm this \
             session's skills"
                .to_string()
        })?
        .map_err(|err| format!("Failed to run the Claude CLI plugin inventory: {err}"))?;
    if !output.status.success() {
        return Err(format!(
            "The Claude CLI refused to list plugins for this session's root {}: {}",
            root.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    verify_plugin_inventory(&String::from_utf8_lossy(&output.stdout), root)
}

/// Does the local CLI advertise `--plugin-dir`?
///
/// Cached per server process. A missing binary or a failed probe is reported as
/// "unsupported" rather than assumed working, so the failure surfaces as a
/// named capability error instead of an opaque CLI exit at spawn time.
async fn claude_supports_plugin_dir() -> bool {
    // The CLI does not change underneath a running server process.
    static SUPPORTED: tokio::sync::OnceCell<bool> = tokio::sync::OnceCell::const_new();
    *SUPPORTED.get_or_init(probe_plugin_dir_support).await
}

async fn probe_plugin_dir_support() -> bool {
    let mut cmd = Command::new(claude_binary());
    cmd.arg("--help");
    if let Some(path) = process_env::resolved_child_process_path() {
        cmd.env("PATH", path);
    }
    match cmd.output().await {
        Ok(output) => {
            let help = String::from_utf8_lossy(&output.stdout);
            let supported = help_text_supports_plugin_dir(&help);
            if !supported {
                tracing::warn!(
                    "Claude CLI --help does not advertise --plugin-dir; Tyde skills cannot be \
                     exposed natively"
                );
            }
            supported
        }
        Err(err) => {
            tracing::warn!("Failed to probe Claude CLI for --plugin-dir support: {err}");
            false
        }
    }
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
        cli_args.push(model_name.clone());
        cli_args.push("--settings".to_string());
        cli_args.push(serde_json::json!({ "availableModels": [model_name] }).to_string());
    }

    if let Some(plugin_root) = config
        .skill_plugin_root
        .as_deref()
        .and_then(normalize_nonempty)
    {
        cli_args.push(CLAUDE_PLUGIN_DIR_FLAG.to_string());
        cli_args.push(plugin_root);
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
            StartupMcpTransport::Http {
                url,
                headers,
                bearer_token_env_var,
            } => {
                let trimmed_url = url.trim();
                if trimmed_url.is_empty() {
                    continue;
                }
                let mut config = serde_json::Map::new();
                config.insert("type".to_string(), Value::String("http".to_string()));
                config.insert("url".to_string(), Value::String(trimmed_url.to_string()));
                let mut headers = headers.clone();
                if let Some(variable) = bearer_token_env_var
                    .as_deref()
                    .map(str::trim)
                    .filter(|variable| !variable.is_empty())
                    && let Ok(token) = std::env::var(variable)
                {
                    headers.retain(|name, _| !name.eq_ignore_ascii_case("authorization"));
                    headers.insert("Authorization".to_owned(), format!("Bearer {token}"));
                }
                if !headers.is_empty() {
                    config.insert(
                        "headers".to_string(),
                        serde_json::to_value(&headers)
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

fn claude_startup_mcp_is_required(name: &str) -> bool {
    matches!(
        name,
        "tyde-config"
            | "tyde-debug"
            | "tyde-agent-control"
            | "tyde-agent-await"
            | "tyde-review-feedback"
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

async fn sync_persistent_background_activity(
    inner: &ClaudeInner,
    subagent_streams: &HashMap<String, SubAgentStream>,
    workflow_runs: &HashMap<String, WorkflowRunEntry>,
) {
    let bash_active = {
        let registry = inner
            .background_tasks
            .lock()
            .expect("Claude background task mutex poisoned");
        registry.owner_active
            && registry
                .entries
                .values()
                .any(|entry| entry.state.status == BackgroundTaskStatus::Running)
    };
    let subagent_active = subagent_streams.values().any(|stream| {
        matches!(
            stream.execution,
            SubAgentExecution::Background | SubAgentExecution::Unknown
        )
    });
    let workflow_active = workflow_runs
        .values()
        .any(|entry| entry.state.status == WorkflowRunStatus::Running);
    let active = bash_active || subagent_active || workflow_active;
    if inner.set_background_work_active(active) && !active {
        inner.emit_idle_if_no_active_turn().await;
    }
}

async fn read_claude_stdout_persistent(
    stdout: ChildStdout,
    inner: Arc<ClaudeInner>,
    control_waiters: ClaudeControlWaiters,
    stdin: Arc<Mutex<ChildStdin>>,
    process_generation: u64,
) {
    let mut turn_state = PersistentStdoutTurnState::default();
    let mut lines = BufReader::new(stdout).lines();
    let mut subagent_streams: HashMap<String, SubAgentStream> = HashMap::new();
    let mut known_subagent_ids = HashSet::new();
    let mut pending_subagent_prompts: HashMap<u64, PendingSubAgentPrompt> = HashMap::new();
    let mut pending_subagent_spawns: HashMap<String, SubAgentSpawnSpec> = HashMap::new();
    let mut local_agent_tasks: HashMap<String, String> = HashMap::new();
    // Keyed by task_id; lives at loop scope (not per-turn) because a
    // workflow's task frames keep arriving after its turn completes.
    let mut workflow_runs: HashMap<String, WorkflowRunEntry> = HashMap::new();
    let mut recent_system_subtypes = DroppedTurnStartLog::default();
    // Frames held back while skill verification is still Pending. The `init`
    // frame arrives at the head of the CLI's response to the first user
    // message, so anything the model says can reach this reader *before* Tyde
    // knows whether the session has the skills it was configured with. Holding
    // them a beat keeps the skill notice ahead of the output it is about.
    //
    // Every exit from the hold releases these frames, including the ones that
    // report a gap: the output is the model's answer either way, and a skill
    // Tyde could not expose is not a reason to make the user's turn vanish.
    let mut held_back: Vec<Value> = Vec::new();
    // Held-back frames are bounded in both count and size. A CLI that streams a
    // long answer and never reports would otherwise grow this without limit,
    // turning a verification problem into a memory problem.
    let mut held_back_bytes: usize = 0;
    // Frames still to process. Normally one line at a time, but a flush after
    // verification pushes the held-back frames back to the front, so replay
    // reuses exactly the same handling as live frames.
    let mut queue: std::collections::VecDeque<Value> = std::collections::VecDeque::new();

    loop {
        let value = match queue.pop_front() {
            Some(value) => value,
            None => {
                let Ok(Some(line)) = lines.next_line().await else {
                    // EOF. A session that never confirmed its skills says so —
                    // unless the user cancelled, in which case this is the
                    // cancel taking effect and blaming the skills would be
                    // wrong.
                    if inner.skills_awaiting_verification() {
                        if inner.active_turn_is_interrupted().await {
                            inner.abandon_skill_verification_for_cancelled_turn().await;
                        } else {
                            inner
                                .report_skill_gap(
                                    &inner.current_skill_failure_target().await,
                                    "Claude exited before reporting which skills it loaded, so \
                                     Tyde could not confirm this session had them.",
                                    true,
                                )
                                .await;
                        }
                    }
                    inner
                        .fail_resume_bootstrap(
                            process_generation,
                            "Claude CLI exited before its resume bootstrap reached a terminal result",
                        )
                        .await;
                    // Release anything still held: it is output the user has
                    // not seen yet, and this is the last chance to show it. The
                    // next pass finds the buffer empty and leaves.
                    if !held_back.is_empty() {
                        for held in held_back.drain(..).rev() {
                            queue.push_front(held);
                        }
                        held_back_bytes = 0;
                        continue;
                    }
                    break;
                };
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                match serde_json::from_str::<Value>(trimmed) {
                    Ok(value) => value,
                    Err(_) => {
                        tracing::warn!("Non-JSON line from Claude CLI: {trimmed}");
                        continue;
                    }
                }
            }
        };

        // Control *responses* are the only inbound traffic that must flow
        // before verification: the `initialize` handshake Tyde itself issued is
        // answered this way, and blocking it would stall the CLI before it
        // could ever emit an `init` frame.
        if route_control_response(&value, &control_waiters).await {
            continue;
        }
        inner.observe_process_metadata(&value).await;
        tracing::debug!(
            process_generation,
            frame_type = value
                .get("type")
                .and_then(|field| field.as_str())
                .unwrap_or(""),
            frame_subtype = value
                .get("subtype")
                .and_then(|field| field.as_str())
                .unwrap_or(""),
            session_id = value
                .get("session_id")
                .and_then(|field| field.as_str())
                .unwrap_or(""),
            result = value
                .get("result")
                .and_then(|field| field.as_str())
                .unwrap_or(""),
            "received Claude CLI frame"
        );
        if let Some(reported) = claude_init_frame_skills(&value) {
            inner.record_skill_init_frame(reported).await;
        }
        if value.get("type").and_then(Value::as_str) == Some("rate_limit_event") {
            inner.handle_passive_capacity(&value).await;
        }
        if inner
            .quarantine_resume_bootstrap_frame(process_generation, &value)
            .await
        {
            continue;
        }

        // A cancelled turn is checked before anything below can call this a
        // skill problem. The user stopping a turn is not the skills failing,
        // and the terminal frames that follow a cancel must report cancellation.
        if inner.skills_awaiting_verification() && inner.active_turn_is_interrupted().await {
            inner.abandon_skill_verification_for_cancelled_turn().await;
            // Requeued behind the frames it followed, and only once the hold is
            // actually over — re-queuing a frame that would take this branch
            // again is a spin, not a replay.
            if !inner.skills_awaiting_verification() {
                release_held_frames(
                    &mut held_back,
                    &mut held_back_bytes,
                    &mut queue,
                    Some(value),
                );
                continue;
            }
        }

        // Inbound control *requests* — permission prompts, ExitPlanMode,
        // AskUserQuestion — are turn-time actions that carry a request id the
        // CLI is blocking on, so they can never be buffered: holding one is a
        // hang. The real protocol does not send them before the `init` frame,
        // so one arriving here means the frame is not coming. Settle
        // verification with a notice and answer the request, rather than
        // leaving the CLI blocked on a reply that will never come.
        if inner.skills_awaiting_verification()
            && value.get("type").and_then(Value::as_str) == Some("control_request")
        {
            let subtype = value
                .pointer("/request/subtype")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            inner
                .report_skill_gap(
                    &inner.current_skill_failure_target().await,
                    &format!(
                        "Claude asked Tyde to act on a '{subtype}' control request before \
                         reporting which skills it loaded, so Tyde could not confirm this \
                         session had them."
                    ),
                    true,
                )
                .await;
            if !inner.skills_awaiting_verification() {
                release_held_frames(
                    &mut held_back,
                    &mut held_back_bytes,
                    &mut queue,
                    Some(value),
                );
                continue;
            }
            // The settle was declined — a cancel or a newer process won the
            // race. Fall through and answer the request regardless: the CLI is
            // blocked on a reply, and leaving it blocked is the one outcome
            // worse than answering under an unconfirmed skill set.
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

        // The `init` frame lists the skills the CLI actually loaded. It is not
        // `continue`d: the frame still flows to the per-turn reducer.
        if let Some(reported) = claude_init_frame_skills(&value) {
            inner.record_skill_init_frame(reported).await;
            // Settled — whether every skill arrived or some did not. Everything
            // held back is genuine output either way, and it arrived *before*
            // this frame, so it goes in front of it and this frame is requeued
            // behind: true arrival order rather than letting the settling frame
            // overtake the output it settled. Guarded on a non-empty buffer so
            // the requeued frame cannot loop through here forever.
            if !held_back.is_empty() && !inner.skills_awaiting_verification() {
                release_held_frames(
                    &mut held_back,
                    &mut held_back_bytes,
                    &mut queue,
                    Some(value),
                );
                continue;
            }
        }
        if inner.skills_awaiting_verification() {
            // A terminal frame while still unverified means the confirmation is
            // never coming. Say so, then let the frame through: this is the end
            // of the user's turn, and swallowing it would strand the turn.
            if claude_frame_is_turn_terminal(&value) {
                inner
                    .report_skill_gap(
                        &inner.current_skill_failure_target().await,
                        "Claude finished its turn without reporting which skills it loaded, so \
                         Tyde could not confirm this session had them.",
                        true,
                    )
                    .await;
                if !inner.skills_awaiting_verification() {
                    release_held_frames(
                        &mut held_back,
                        &mut held_back_bytes,
                        &mut queue,
                        Some(value),
                    );
                    continue;
                }
                // Settle declined by a racing cancel or a newer process. Hold
                // this frame with the rest rather than re-queueing it into the
                // same branch; EOF releases the buffer.
                held_back_bytes += value.to_string().len();
                held_back.push(value);
                continue;
            }
            let frame_bytes = value.to_string().len();
            if held_back.len() + 1 > CLAUDE_HELD_BACK_FRAME_LIMIT
                || held_back_bytes + frame_bytes > CLAUDE_HELD_BACK_BYTE_LIMIT
            {
                // The buffer is a courtesy — it keeps the notice ahead of the
                // output it describes — and it is not worth unbounded memory or
                // a stalled stream. Stop holding, say why, and let the output
                // flow from here on.
                inner
                    .report_skill_gap(
                        &inner.current_skill_failure_target().await,
                        &format!(
                            "Claude produced more than {} frames or {} bytes of output without \
                             reporting which skills it loaded, so Tyde could not confirm this \
                             session had them.",
                            CLAUDE_HELD_BACK_FRAME_LIMIT, CLAUDE_HELD_BACK_BYTE_LIMIT
                        ),
                        true,
                    )
                    .await;
                // Released unconditionally: the bound has to hold whether or not
                // the settle took, and an empty buffer means the requeued frame
                // is under the limit next time round rather than back here.
                release_held_frames(
                    &mut held_back,
                    &mut held_back_bytes,
                    &mut queue,
                    Some(value),
                );
                continue;
            }
            held_back_bytes += frame_bytes;
            held_back.push(value);
            continue;
        }

        // Arm before the task handlers below consume the frame. A completing
        // task is what makes the CLI wake the model, so this has to see the
        // notification whether it belongs to a workflow or a background task.
        if claude_frame_arms_cli_wake(&value) {
            inner.arm_cli_wake();
        }
        recent_system_subtypes.observe(&value);
        if value.get("type").and_then(Value::as_str) == Some("system")
            && value.get("subtype").and_then(Value::as_str) == Some("api_retry")
        {
            eprintln!("TYDE CLAUDE RAW API RETRY frame={value}");
        }

        if handle_workflow_task_frame(&value, &mut workflow_runs, &inner.emitter) {
            sync_persistent_background_activity(&inner, &subagent_streams, &workflow_runs).await;
            continue;
        }
        let handled_background_task = inner.handle_background_task_frame(&value, &subagent_streams);
        if handled_background_task {
            finalize_ready_background_subagents(&mut subagent_streams);
            sync_persistent_background_activity(&inner, &subagent_streams, &workflow_runs).await;
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
            observe_local_agent_task_usage(
                &inner,
                &value,
                &mut local_agent_tasks,
                &mut subagent_streams,
            );
            known_subagent_ids.extend(subagent_streams.keys().cloned());
            // A background sub-agent completes via `task_notification`, which
            // arrives on the parent stream after the parent's turn `result`.
            // Handle it pre-gate so it lands even with no active turn.
            finalize_background_subagent_completion(&value, &mut subagent_streams);
            sync_persistent_background_activity(&inner, &subagent_streams, &workflow_runs).await;
        }

        if let Some(parent_id) = extract_parent_tool_use_id(&value) {
            handle_correlated_subagent_event(
                &mut subagent_streams,
                &known_subagent_ids,
                &inner.emitter,
                parent_id,
                &value,
            );
            {
                let mut background_tasks = inner
                    .background_tasks
                    .lock()
                    .expect("Claude background task mutex poisoned");
                refresh_unresolved_background_tasks(
                    &value,
                    &mut background_tasks.entries,
                    &inner.emitter,
                    &subagent_streams,
                );
            }
            sync_persistent_background_activity(&inner, &subagent_streams, &workflow_runs).await;
            continue;
        }

        let _turn_event_guard = inner.turn_event_gate.lock().await;
        let (turn_id, model_hint, owner) =
            match prepare_persistent_stdout_turn(&inner, &mut turn_state).await {
                Some(turn) => turn,
                None => {
                    // No user-initiated turn is active, yet the CLI is emitting
                    // fresh turn content. That happens when a completed
                    // background task wakes the model: the launching turn's
                    // `result` already landed, then a new init + assistant +
                    // `result` sequence arrives. It is the model's own turn —
                    // it can call tools, not just narrate — so adopt it rather
                    // than drop it. The wake token is what separates this from
                    // a stray frame trailing a finished turn.
                    if !is_cli_turn_start_event(&value) {
                        recent_system_subtypes.warn_once_on_dropped_frame(&value);
                        continue;
                    }
                    if !inner.has_pending_cli_wake() {
                        recent_system_subtypes.warn_once_on_dropped_turn_start(&value);
                        continue;
                    }
                    if inner.begin_cli_initiated_turn().await.is_none() {
                        // The finished turn is still installed because its
                        // finalizer runs concurrently. Wait for the hand-off:
                        // the CLI can deliver every frame of the wake burst
                        // inside that window, so retrying on the next frame is
                        // not enough to save the turn.
                        if !inner
                            .await_active_turn_quiesced(CLAUDE_WAKE_QUIESCE_WAIT)
                            .await
                            || inner.begin_cli_initiated_turn().await.is_none()
                        {
                            // The wake could not be adopted even after waiting
                            // for the hand-off. Leave the token armed so a
                            // later frame can retry, but make this failure mode
                            // visible.
                            tracing::warn!(
                                wait_secs = CLAUDE_WAKE_QUIESCE_WAIT.as_secs(),
                                "could not open a turn for a Claude wake; the previous turn \
                                 did not hand off in time or the backend is closing"
                            );
                            continue;
                        }
                    }
                    inner.take_pending_cli_wake();
                    match prepare_persistent_stdout_turn(&inner, &mut turn_state).await {
                        Some(turn) => turn,
                        None => continue,
                    }
                }
            };

        if matches!(owner, ClaudeTurnOwner::Compaction(_)) {
            inner.observe_compaction_frame(turn_id, &value).await;
            if value.get("type").and_then(Value::as_str) == Some("result") {
                turn_state.active_turn_id = None;
                turn_state.base_message_id.clear();
                turn_state.current_message_id.clear();
                let _ = inner
                    .finish_compaction(
                        turn_id,
                        BackendCompactionFailureKind::ProtocolViolation,
                        None,
                        false,
                    )
                    .await;
            }
            continue;
        }

        let interrupt_requested = inner.active_turn_interrupted(turn_id).await;
        if value.get("type").and_then(Value::as_str) == Some("user")
            && phase_has_pending_output(&turn_state.summary, &turn_state.segment)
        {
            close_current_phase(&mut turn_state.summary, &mut turn_state.segment, &inner);
            flush_ready_workflow_snapshots(&mut workflow_runs, &inner.emitter);
        }
        if subagent_emitter.is_some() {
            detect_subagent_completions(&value, &mut subagent_streams).await;
            sync_persistent_background_activity(&inner, &subagent_streams, &workflow_runs).await;
        }
        consume_claude_stream_value_with_interrupt(
            &value,
            &mut turn_state.summary,
            &mut turn_state.segment,
            &inner,
            &turn_state.base_message_id,
            &mut turn_state.current_message_id,
            interrupt_requested,
        );
        flush_ready_workflow_snapshots(&mut workflow_runs, &inner.emitter);
        if let Some(ref emitter) = subagent_emitter {
            flush_pending_subagent_spawns(
                emitter.as_ref(),
                &inner.emitter,
                &mut subagent_streams,
                &mut pending_subagent_spawns,
            )
            .await;
            detect_subagent_spawns(
                &value,
                emitter.as_ref(),
                &inner.emitter,
                &mut subagent_streams,
                &mut pending_subagent_prompts,
                &mut pending_subagent_spawns,
            )
            .await;
            finalize_ready_background_subagents(&mut subagent_streams);
            known_subagent_ids.extend(subagent_streams.keys().cloned());
            sync_persistent_background_activity(&inner, &subagent_streams, &workflow_runs).await;
        }
        {
            let mut background_tasks = inner
                .background_tasks
                .lock()
                .expect("Claude background task mutex poisoned");
            refresh_unresolved_background_tasks(
                &value,
                &mut background_tasks.entries,
                &inner.emitter,
                &subagent_streams,
            );
        }

        if value.get("type").and_then(Value::as_str) == Some("result") {
            flush_pending_tool_uses_with_fallback(&mut turn_state.summary, &mut turn_state.segment);
            flush_ready_workflow_snapshots(&mut workflow_runs, &inner.emitter);
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
        finalize_subagent_stream(stream, SubAgentFinalOutcome::default());
    }
    inner.set_background_work_active(false);

    fail_pending_control_waiters(&control_waiters, "Claude CLI process exited").await;
    let _turn_event_guard = inner.turn_event_gate.lock().await;
    let active_turn_id = if let Some(turn_id) = turn_state.active_turn_id {
        Some(turn_id)
    } else {
        inner.active_turn_pending_outcome_id().await
    };
    if let Some(turn_id) = active_turn_id {
        if matches!(
            inner.active_turn_owner(turn_id).await,
            Some(ClaudeTurnOwner::Compaction(_))
        ) {
            let _ = inner
                .finish_compaction(
                    turn_id,
                    BackendCompactionFailureKind::TransportClosed,
                    Some("Claude process exited before returning a compaction result".to_string()),
                    false,
                )
                .await;
            inner.mark_process_exited().await;
            return;
        }
        if turn_state.active_turn_id.is_some() {
            flush_pending_tool_uses_with_fallback(&mut turn_state.summary, &mut turn_state.segment);
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

/// Whether a frame is the kind of completion that makes the CLI wake the
/// model and run a turn of its own. Any `task_notification` qualifies — the
/// CLI only emits one when a task reaches a terminal state — but a task owned
/// by a sub-agent wakes that sub-agent's stream, not the root, so anything
/// carrying a parent tool id is excluded. Status strings are deliberately not
/// matched: an unrecognized one must not silently cost us the wake turn.
fn claude_frame_arms_cli_wake(value: &Value) -> bool {
    value.get("type").and_then(Value::as_str) == Some("system")
        && value.get("subtype").and_then(Value::as_str) == Some("task_notification")
        && background_task_parent_tool_use_id(value).is_none()
}

/// Whether a parent-stream frame (already excluded from sub-agent routing)
/// marks the start of fresh turn content. Deliberately excludes lone `result`
/// and `user` frames so a stray terminal frame never spawns an empty turn.
/// This is only the frame *shape*: adoption also requires an armed wake token.
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

/// Bounded record of the `system` subtypes seen since the last turn-terminal
/// frame, so that dropping turn-start content without an armed wake token is
/// reported *with the trigger that preceded it*. A dropped wake is roughly
/// eight frames, so this warns once per burst rather than once per frame:
/// the silent `continue` that this replaces is why the original regression
/// went unnoticed for six weeks.
#[derive(Default)]
struct DroppedTurnStartLog {
    recent: VecDeque<String>,
    warned: bool,
    warned_non_start: bool,
}

impl DroppedTurnStartLog {
    const RECENT_LIMIT: usize = 6;

    fn observe(&mut self, value: &Value) {
        if value.get("type").and_then(Value::as_str) == Some("result") {
            self.recent.clear();
            self.warned = false;
            self.warned_non_start = false;
            return;
        }
        if value.get("type").and_then(Value::as_str) != Some("system") {
            return;
        }
        let Some(subtype) = value.get("subtype").and_then(Value::as_str) else {
            return;
        };
        if self.recent.len() == Self::RECENT_LIMIT {
            self.recent.pop_front();
        }
        self.recent.push_back(subtype.to_string());
    }

    fn warn_once_on_dropped_turn_start(&mut self, value: &Value) {
        if self.warned {
            return;
        }
        self.warned = true;
        // Resolved before the macro: inside it, `Value` names `tracing`'s own
        // field trait rather than `serde_json::Value`.
        let frame_type = value.get("type").and_then(Value::as_str).unwrap_or("");
        tracing::warn!(
            frame_type,
            recent_system_subtypes = ?self.recent,
            "dropping Claude turn-start content with no active turn and no armed wake \
             trigger; if this is a real CLI wake, its trigger is not yet recognized"
        );
    }

    /// Frames that are not turn-start shaped and arrive with no turn to own
    /// them are discarded. Most are inert, but a `user` (tool_result) or
    /// `result` frame landing here means a wake turn was missed upstream and
    /// its remainder is being thrown away — the same shape as the regression
    /// this module exists to prevent, so it must not be silent either.
    fn warn_once_on_dropped_frame(&mut self, value: &Value) {
        if self.warned_non_start {
            return;
        }
        self.warned_non_start = true;
        let frame_type = value.get("type").and_then(Value::as_str).unwrap_or("");
        let subtype = value.get("subtype").and_then(Value::as_str).unwrap_or("");
        tracing::warn!(
            frame_type,
            subtype,
            recent_system_subtypes = ?self.recent,
            "discarding a Claude frame with no turn to own it"
        );
    }
}

async fn prepare_persistent_stdout_turn(
    inner: &Arc<ClaudeInner>,
    turn_state: &mut PersistentStdoutTurnState,
) -> Option<(u64, Option<String>, ClaudeTurnOwner)> {
    let (turn_id, model_hint, owner) = {
        let state = inner.state.lock().await;
        let active = state.active_turn.as_ref()?;
        let owner = active.owner.clone();
        // A user turn that has already handed off its outcome is finished; the
        // finalizer that clears it runs concurrently with this reader, so
        // without this check a wake turn arriving inside that window would be
        // consumed against a message id the finalizer has already closed.
        // Compaction turns have no user outcome channel and remain routable
        // until their correlated terminal frame is observed.
        if matches!(owner, ClaudeTurnOwner::User) {
            active.outcome_tx.as_ref()?;
        }
        (active.id, state.model.clone(), owner)
    };
    if turn_state.active_turn_id != Some(turn_id) {
        let base_message_id = format!("claude-msg-{turn_id}");
        turn_state.active_turn_id = Some(turn_id);
        turn_state.base_message_id = base_message_id.clone();
        turn_state.current_message_id = base_message_id;
        turn_state.summary = ClaudeStdoutSummary::default();
        turn_state.segment = SegmentState::default();
    }

    Some((turn_id, model_hint, owner))
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
        let _ = inner.emit_tool_request(&tool_call);
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
        let _ = inner.emit_tool_request(&tool_call);
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
        eprintln!("TYDE CLAUDE STDERR {line}");
        tracing::debug!("Claude stderr: {line}");
        inner.emitter.subprocess_stderr(&line);
    }
}

fn consume_subagent_event(stream: &mut SubAgentStream, value: &Value) {
    let failed_tool_result = value.get("type").and_then(Value::as_str) == Some("user")
        && value
            .pointer("/message/content")
            .and_then(Value::as_array)
            .is_some_and(|content| {
                content.iter().any(|block| {
                    block.get("type").and_then(Value::as_str) == Some("tool_result")
                        && block.get("is_error").and_then(Value::as_bool) == Some(true)
                })
            });
    let failed_task = value.get("type").and_then(Value::as_str) == Some("system")
        && matches!(
            value.get("subtype").and_then(Value::as_str),
            Some("task_updated" | "task_notification")
        )
        && matches!(
            value
                .get("status")
                .or_else(|| value.pointer("/patch/status"))
                .and_then(Value::as_str),
            Some("failed" | "error" | "killed" | "stopped")
        );
    if failed_tool_result || failed_task {
        stream.execution_failed = true;
    }
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
    for tool in &stream.summary.tool_calls {
        stream.seen_tool_call_ids.insert(tool.id.clone());
        stream.last_tool_name = Some(tool.name.clone());
    }
    maybe_emit_subagent_progress(stream);
    flush_subagent_progress(stream);
}

/// Minimum interval between live-status updates on the parent's Task
/// tool card while routing a sub-agent's events.
const SUBAGENT_PROGRESS_EMIT_INTERVAL: Duration = Duration::from_millis(250);
const RESUME_REPLAY_SETTLE_QUIET: Duration = Duration::from_secs(1);
const RESUME_REPLAY_TURN_QUIESCE: Duration = Duration::from_secs(30);

fn subagent_progress_data(stream: &mut SubAgentStream, completed: bool) -> ToolProgressData {
    // The emitted request owns the card's identity. The cached name can be
    // stale or a guess: lazy `task_started` registration may run before or
    // after the tool_use block is seen, and the anchoring call is not always
    // a Task/Agent spawn (a SendMessage continuation anchors local_agent
    // frames to the SendMessage call). Re-resolve on every emission so the
    // frame always matches the request card it decorates.
    if let Some(name) = stream
        .parent_emitter
        .tool_request_name(&stream.parent_tool_use_id)
    {
        stream.parent_tool_name = name;
    }
    ToolProgressData {
        tool_call_id: stream.parent_tool_use_id.clone(),
        execution_mode: if stream.execution == SubAgentExecution::Background
            || stream
                .parent_emitter
                .is_tool_background(&stream.parent_tool_use_id)
        {
            ToolExecutionMode::Background
        } else {
            ToolExecutionMode::Foreground
        },
        update: ToolProgressUpdate::SubAgent(protocol::SubAgentProgress {
            agent_id: stream.agent_id.clone(),
            agent_name: stream.agent_name.clone(),
            last_tool_name: stream.last_tool_name.clone(),
            tool_calls: stream.seen_tool_call_ids.len() as u64,
            completed,
            status: if !completed {
                protocol::SubAgentProgressStatus::Running
            } else if stream.execution_failed {
                protocol::SubAgentProgressStatus::Failed
            } else {
                protocol::SubAgentProgressStatus::Completed
            },
        }),
    }
}

fn maybe_emit_subagent_progress(stream: &mut SubAgentStream) {
    if stream.last_progress_emit.elapsed() < SUBAGENT_PROGRESS_EMIT_INTERVAL {
        return;
    }
    stream.last_progress_emit = std::time::Instant::now();
    queue_subagent_progress(stream, false);
    flush_subagent_progress(stream);
}

fn queue_subagent_progress(stream: &mut SubAgentStream, completed: bool) {
    let progress = subagent_progress_data(stream, completed);
    if !completed
        && stream
            .pending_parent_progress
            .back()
            .is_some_and(|pending| {
                matches!(
                    &pending.update,
                    ToolProgressUpdate::SubAgent(protocol::SubAgentProgress {
                        completed: false,
                        ..
                    })
                )
            })
    {
        stream.pending_parent_progress.pop_back();
    }
    stream.pending_parent_progress.push_back(progress);
}

fn flush_subagent_progress(stream: &mut SubAgentStream) -> bool {
    if !stream
        .parent_emitter
        .has_known_tool_request(&stream.parent_tool_use_id)
    {
        return false;
    }
    for progress in stream.pending_parent_progress.drain(..) {
        stream.parent_emitter.tool_progress(&progress);
    }
    true
}

#[derive(Clone)]
struct SubAgentSpawnSpec {
    tool_use_id: String,
    parent_tool_name: Option<String>,
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
        parent_tool_name,
        name,
        description,
        agent_type,
        session_id_hint,
        execution,
    } = spec;
    if let Some(stream) = streams.get_mut(&tool_use_id) {
        if execution != SubAgentExecution::Unknown {
            stream.execution = execution;
        }
        if let Some(parent_tool_name) = parent_tool_name {
            stream.parent_tool_name = parent_tool_name;
        }
        if crate::sub_agent::child_name_is_better(&stream.agent_name, &name) {
            stream.agent_name = name.clone();
            if let Some(tx) = &stream.name_update_tx {
                let _ = tx.send(name);
            }
            queue_subagent_progress(stream, false);
            flush_subagent_progress(stream);
        }
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
    spawn_claude_subagent_event_bridge(
        raw_event_rx,
        handle.event_tx.clone(),
        handle.model_usage_tx.clone(),
        handle.total_usage_tx.clone(),
    );

    // Create a ClaudeInner that routes events to the sub-agent's channel.
    let sa_inner = Arc::new(ClaudeInner {
        emitter: Arc::new(TurnEmitter::new_for_agent(
            raw_event_tx,
            AgentName(CLAUDE_AGENT_NAME),
        )),
        active_response: StdMutex::new(None),
        state: Mutex::new(ClaudeState::default()),
        runtime: Mutex::new(None),
        turn_event_gate: Mutex::new(()),
        task_tracker: StdMutex::new(ClaudeTaskTracker::default()),
        background_tasks: StdMutex::new(BackgroundTaskRegistry::active()),
        native_subagent_tasks: StdMutex::new(HashSet::new()),
        skill_readiness: watch::channel(ClaudeSkillReadiness::NotRequired).0,
        skill_verification_abandoned: std::sync::atomic::AtomicBool::new(false),
        pending_cli_wake: std::sync::atomic::AtomicBool::new(false),
        background_work_active: std::sync::atomic::AtomicBool::new(false),
        typing_active: std::sync::atomic::AtomicBool::new(false),
    });
    let sa_message_id = format!("subagent-{}", tool_use_id);

    // Relay agents start active in the host registry, so mirror that state in
    // the child emitter before any streamed output arrives. The terminal
    // typing(false) emitted by `finalize_subagent_stream` must be observable;
    // otherwise TurnEmitter correctly deduplicates it against its default
    // idle state and the rendered child remains active forever.
    sa_inner.emit_typing_status(true);

    let mut stream = SubAgentStream {
        summary: ClaudeStdoutSummary::default(),
        segment: SegmentState {
            awaiting_stream_start: true,
            ..SegmentState::default()
        },
        message_id: sa_message_id,
        inner: sa_inner,
        parent_tool_use_id: tool_use_id.clone(),
        // Prefer the caller-observed block name, then the emitted request
        // (lazy task_started registration may anchor to any agent-control
        // call, e.g. a SendMessage continuation). "Task" is only the seed
        // for the request-not-yet-seen race; progress emission re-resolves
        // it against the request registry every time.
        parent_tool_name: parent_tool_name
            .or_else(|| parent_emitter.tool_request_name(&tool_use_id))
            .unwrap_or_else(|| "Task".to_owned()),
        agent_id: handle.agent_id,
        agent_name: name,
        name_update_tx: handle.name_update_tx,
        parent_emitter: parent_emitter.clone(),
        last_progress_emit: std::time::Instant::now(),
        execution,
        seen_tool_call_ids: HashSet::new(),
        last_tool_name: None,
        reported_total_tokens: None,
        execution_failed: false,
        pending_terminal: None,
        pending_parent_progress: VecDeque::new(),
    };
    // Unthrottled spawn update: the Task card learns the sub-agent's id
    // (for its "Open agent" link) as soon as the agent exists.
    queue_subagent_progress(&mut stream, false);
    flush_subagent_progress(&mut stream);
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
    pending_snapshots: VecDeque<WorkflowRunState>,
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

fn queue_workflow_snapshot(entry: &mut WorkflowRunEntry) {
    let snapshot = entry.state.clone();
    if snapshot.status == WorkflowRunStatus::Running {
        match entry.pending_snapshots.len() {
            0 => entry.pending_snapshots.push_back(snapshot),
            1 if entry.pending_snapshots.back() != Some(&snapshot) => {
                entry.pending_snapshots.push_back(snapshot);
            }
            2.. if entry
                .pending_snapshots
                .back()
                .is_some_and(|queued| queued.status == WorkflowRunStatus::Running) =>
            {
                entry.pending_snapshots.pop_back();
                entry.pending_snapshots.push_back(snapshot);
            }
            _ => {}
        }
    } else if entry.pending_snapshots.back() != Some(&snapshot) {
        if entry
            .pending_snapshots
            .back()
            .is_some_and(|queued| queued.status != WorkflowRunStatus::Running)
        {
            entry.pending_snapshots.pop_back();
        }
        entry.pending_snapshots.push_back(snapshot);
    }

    while entry.pending_snapshots.len() > 3 {
        entry.pending_snapshots.remove(1);
    }
}

fn flush_workflow_snapshots(emitter: &TurnEmitter, entry: &mut WorkflowRunEntry) -> bool {
    let request_known = emitter.has_known_tool_request(&entry.tool_use_id);
    eprintln!(
        "CLAUDE WORKFLOW FLUSH tool={} known={} queued={} status={:?}",
        entry.tool_use_id,
        request_known,
        entry.pending_snapshots.len(),
        entry.state.status
    );
    if !request_known {
        tracing::debug!(
            tool_use_id = entry.tool_use_id,
            queued_snapshots = entry.pending_snapshots.len(),
            status = ?entry.state.status,
            "buffering Workflow progress until its tool request is emitted"
        );
        return false;
    }
    if !entry.pending_snapshots.is_empty() {
        tracing::debug!(
            tool_use_id = entry.tool_use_id,
            queued_snapshots = entry.pending_snapshots.len(),
            "flushing buffered Workflow progress after its tool request"
        );
    }
    for snapshot in entry.pending_snapshots.drain(..) {
        emitter.tool_progress(&ToolProgressData {
            tool_call_id: entry.tool_use_id.clone(),
            execution_mode: ToolExecutionMode::Foreground,
            update: ToolProgressUpdate::Workflow(snapshot),
        });
    }
    true
}

fn emit_workflow_snapshot(emitter: &TurnEmitter, entry: &mut WorkflowRunEntry) -> bool {
    entry.last_emit = std::time::Instant::now();
    queue_workflow_snapshot(entry);
    flush_workflow_snapshots(emitter, entry)
}

fn flush_ready_workflow_snapshots(
    workflow_runs: &mut HashMap<String, WorkflowRunEntry>,
    emitter: &TurnEmitter,
) {
    let mut completed = Vec::new();
    for (task_id, entry) in workflow_runs.iter_mut() {
        if flush_workflow_snapshots(emitter, entry)
            && entry.pending_snapshots.is_empty()
            && entry.state.status != WorkflowRunStatus::Running
        {
            completed.push(task_id.clone());
        }
    }
    for task_id in completed {
        workflow_runs.remove(&task_id);
    }
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
                pending_snapshots: VecDeque::new(),
            };
            eprintln!(
                "CLAUDE WORKFLOW START task={} tool={} name={}",
                task_id, entry.tool_use_id, entry.state.workflow_name
            );
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
            eprintln!(
                "CLAUDE WORKFLOW PROGRESS task={} tool={}",
                task_id, entry.tool_use_id
            );
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
            let Some(entry) = workflow_runs.get_mut(&task_id) else {
                return false;
            };
            eprintln!(
                "CLAUDE WORKFLOW TERMINAL task={} tool={} raw_status={:?}",
                task_id, entry.tool_use_id, system.status
            );
            entry.state.status = match system.status.as_deref() {
                Some("completed") => WorkflowRunStatus::Completed,
                Some("failed") | Some("error") => WorkflowRunStatus::Failed,
                other => {
                    tracing::warn!("unknown workflow task_notification status: {other:?}");
                    WorkflowRunStatus::Unknown
                }
            };
            entry.state.summary = system.summary.as_deref().and_then(normalize_nonempty);
            emit_workflow_snapshot(emitter, entry);
            if entry.pending_snapshots.is_empty() {
                workflow_runs.remove(&task_id);
            } else {
                tracing::debug!(
                    task_id,
                    tool_use_id = entry.tool_use_id,
                    queued_snapshots = entry.pending_snapshots.len(),
                    "retaining terminal Workflow progress until its tool request is emitted"
                );
            }
            true
        }
        _ => false,
    }
}

struct BackgroundTaskEntry {
    tool_use_id: String,
    tool_name: Option<String>,
    owner: Option<Arc<TurnEmitter>>,
    parent_tool_use_id: Option<String>,
    state: BackgroundTaskState,
    output: Option<ClaudeRunCommandResult>,
    output_path: Option<String>,
    terminal_notification_received: bool,
}

fn background_task_parent_tool_use_id(value: &Value) -> Option<&str> {
    extract_parent_tool_use_id(value)
        .or_else(|| {
            value
                .pointer("/data/parent_tool_use_id")
                .and_then(Value::as_str)
        })
        .or_else(|| {
            value
                .pointer("/message/parent_tool_use_id")
                .and_then(Value::as_str)
        })
        .filter(|id| !id.is_empty())
}

fn resolve_background_task_owner(
    tool_use_id: &str,
    parent_tool_use_id: Option<&str>,
    root_emitter: &Arc<TurnEmitter>,
    subagent_streams: &HashMap<String, SubAgentStream>,
) -> Option<Arc<TurnEmitter>> {
    let explicit_owner = parent_tool_use_id.and_then(|parent_id| subagent_streams.get(parent_id));
    let inferred_owners = subagent_streams
        .values()
        .filter(|stream| {
            stream
                .inner
                .emitter
                .tool_request_name(tool_use_id)
                .is_some()
                || stream.summary.tool_call_by_id.contains_key(tool_use_id)
                || stream
                    .summary
                    .tool_calls
                    .iter()
                    .any(|tool| tool.id == tool_use_id)
        })
        .collect::<Vec<_>>();
    if inferred_owners.len() == 1 {
        return Some(Arc::clone(&inferred_owners[0].inner.emitter));
    }
    if inferred_owners.len() > 1 {
        return None;
    }
    if let Some(stream) = explicit_owner {
        return Some(Arc::clone(&stream.inner.emitter));
    }
    root_emitter
        .tool_request_name(tool_use_id)
        .is_some()
        .then(|| Arc::clone(root_emitter))
}

fn refresh_background_task_owner(
    value: &Value,
    entry: &mut BackgroundTaskEntry,
    root_emitter: &Arc<TurnEmitter>,
    subagent_streams: &HashMap<String, SubAgentStream>,
) {
    if let Some(parent_tool_use_id) = background_task_parent_tool_use_id(value) {
        entry.parent_tool_use_id = Some(parent_tool_use_id.to_owned());
    }
    let frame_tool_name = collect_tool_use_blocks(value)
        .into_iter()
        .find_map(|block| {
            (block.get("id").and_then(Value::as_str) == Some(entry.tool_use_id.as_str()))
                .then(|| block.get("name").and_then(Value::as_str))
                .flatten()
                .and_then(normalize_nonempty)
        });
    if entry.owner.is_none() && frame_tool_name.is_some() {
        entry.owner = match entry.parent_tool_use_id.as_deref() {
            Some(parent_id) => subagent_streams
                .get(parent_id)
                .map(|stream| Arc::clone(&stream.inner.emitter)),
            None => Some(Arc::clone(root_emitter)),
        };
    }
    if entry.owner.is_some() && entry.tool_name.is_none() {
        entry.tool_name = frame_tool_name;
    }
    if entry.owner.is_none() {
        entry.owner = resolve_background_task_owner(
            &entry.tool_use_id,
            entry.parent_tool_use_id.as_deref(),
            root_emitter,
            subagent_streams,
        );
    }
    if entry.tool_name.is_none() {
        entry.tool_name = entry
            .owner
            .as_deref()
            .and_then(|owner| owner.tool_request_name(&entry.tool_use_id));
    }
    if let Some(command) = entry
        .owner
        .as_deref()
        .and_then(|owner| owner.tool_request_command(&entry.tool_use_id))
    {
        entry.state.description = Some(command);
    }
}

fn refresh_unresolved_background_tasks(
    value: &Value,
    background_tasks: &mut HashMap<String, BackgroundTaskEntry>,
    root_emitter: &Arc<TurnEmitter>,
    subagent_streams: &HashMap<String, SubAgentStream>,
) {
    let mut resolved_terminals = Vec::new();
    for entry in background_tasks
        .values_mut()
        .filter(|entry| entry.owner.is_none() || entry.tool_name.is_none())
    {
        let was_ready = entry.owner.is_some() && entry.tool_name.is_some();
        refresh_background_task_owner(value, entry, root_emitter, subagent_streams);
        if !was_ready
            && let (Some(owner), Some(_)) = (entry.owner.as_deref(), entry.tool_name.as_deref())
        {
            if entry.state.status == BackgroundTaskStatus::Running {
                emit_background_task_snapshot(owner, entry);
            } else if entry.terminal_notification_received {
                emit_background_task_completion(owner, entry);
                resolved_terminals.push(entry.state.task_id.clone());
            }
        }
    }
    for task_id in resolved_terminals {
        background_tasks.remove(&task_id);
    }
}

const BACKGROUND_COMMAND_OUTPUT_LIMIT: u64 = 64 * 1024;

fn capture_background_command_output(
    output_file: Option<&str>,
) -> Result<ClaudeRunCommandResult, &'static str> {
    let Some(raw_path) = output_file.map(str::trim).filter(|path| !path.is_empty()) else {
        return Err("Claude did not provide a structured command output file");
    };
    let temp_root = std::fs::canonicalize(std::env::temp_dir())
        .map_err(|_| "Claude command output file was unavailable")?;
    let path = std::fs::canonicalize(raw_path)
        .map_err(|_| "Claude command output file was unavailable")?;
    if !path.starts_with(&temp_root) {
        return Err("Claude command output path was outside the temporary directory");
    }
    let metadata =
        std::fs::metadata(&path).map_err(|_| "Claude command output file was unavailable")?;
    if !metadata.is_file() {
        return Err("Claude command output file was unavailable");
    }
    if metadata.len() > BACKGROUND_COMMAND_OUTPUT_LIMIT {
        return Err("Claude command output exceeded the capture limit");
    }
    let file =
        std::fs::File::open(path).map_err(|_| "Claude command output file was unavailable")?;
    let mut bytes = Vec::with_capacity(BACKGROUND_COMMAND_OUTPUT_LIMIT as usize);
    file.take(BACKGROUND_COMMAND_OUTPUT_LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "Claude command output file was unavailable")?;
    if bytes.len() as u64 > BACKGROUND_COMMAND_OUTPUT_LIMIT {
        return Err("Claude command output exceeded the capture limit");
    }
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|_| "Claude command output was not structurally available")?;
    let Value::Object(map) = &value else {
        return Err("Claude command output was not structurally available");
    };
    let has_structured_field = [
        "exit_code",
        "exitCode",
        "code",
        "return_code",
        "returnCode",
        "stdout",
        "output",
        "std_out",
        "stderr",
        "error",
        "std_err",
    ]
    .iter()
    .any(|key| map.contains_key(*key));
    if !has_structured_field {
        return Err("Claude command output was not structurally available");
    }
    Ok(
        parse_run_command_result_from_value(&value, 0).unwrap_or(ClaudeRunCommandResult {
            exit_code: 0,
            stdout: String::new(),
            stderr: String::new(),
        }),
    )
}

fn emit_background_task_snapshot(emitter: &TurnEmitter, entry: &BackgroundTaskEntry) {
    if entry.tool_name.is_none() || entry.state.status != BackgroundTaskStatus::Running {
        return;
    }
    emitter.tool_progress(&ToolProgressData {
        tool_call_id: entry.tool_use_id.clone(),
        execution_mode: ToolExecutionMode::Background,
        update: ToolProgressUpdate::Other {
            payload: json!({
                "task_id": entry.state.task_id,
                "description": entry.state.description,
                "summary": entry.state.summary,
            }),
        },
    });
}

fn emit_background_task_completion(emitter: &TurnEmitter, entry: &BackgroundTaskEntry) {
    let Some(tool_name) = entry.tool_name.as_deref() else {
        return;
    };
    let outcome = if let Some(result) = entry.output.as_ref()
        && entry.state.status != BackgroundTaskStatus::Stopped
    {
        if result.exit_code == 0 {
            ToolExecutionOutcome::Succeeded {
                result: ToolExecutionResult::RunCommand {
                    exit_code: result.exit_code,
                    stdout: result.stdout.clone(),
                    stderr: result.stderr.clone(),
                },
            }
        } else {
            ToolExecutionOutcome::Failed {
                message: "Background command exited non-zero".to_owned(),
                details: Some(if result.stderr.trim().is_empty() {
                    result.stdout.clone()
                } else {
                    result.stderr.clone()
                }),
                normalization_failure: None,
            }
        }
    } else {
        match entry.state.status {
            BackgroundTaskStatus::Completed => ToolExecutionOutcome::Succeeded {
                result: ToolExecutionResult::RunCommand {
                    exit_code: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                },
            },
            BackgroundTaskStatus::Stopped => ToolExecutionOutcome::Cancelled {
                message: entry
                    .state
                    .output_unavailable
                    .clone()
                    .unwrap_or_else(|| "Background command stopped".to_owned()),
            },
            BackgroundTaskStatus::Failed | BackgroundTaskStatus::Unknown => {
                let message = entry
                    .state
                    .summary
                    .as_deref()
                    .and_then(normalize_nonempty)
                    .unwrap_or_else(|| "Background command failed".to_owned());
                ToolExecutionOutcome::Failed {
                    message: first_line_trimmed(&message, 140),
                    details: Some(message),
                    normalization_failure: None,
                }
            }
            BackgroundTaskStatus::Running => return,
        }
    };
    let _ = emit_tool_completion_for_known_request(emitter, &entry.tool_use_id, tool_name, outcome);
}

fn emit_tool_completion_for_known_request(
    emitter: &TurnEmitter,
    tool_call_id: &str,
    tool_name: &str,
    outcome: ToolExecutionOutcome,
) -> bool {
    if !emitter.has_pending_tool_request(tool_call_id) {
        tracing::error!(
            tool_call_id,
            tool_name,
            "Claude tool completion had no pending declared request"
        );
        emitter.backend_error(
            "Claude emitted a tool completion without a pending declared provider-response request",
        );
        return false;
    }
    emitter.tool_completed(tool_call_id, outcome);
    true
}

fn drain_background_task_entries(background_tasks: &mut HashMap<String, BackgroundTaskEntry>) {
    for (_, mut entry) in background_tasks.drain() {
        let Some(owner) = entry.owner.as_deref() else {
            continue;
        };
        if entry.tool_name.is_none() {
            continue;
        }
        if !entry.terminal_notification_received {
            entry.state.status = BackgroundTaskStatus::Stopped;
            entry.state.summary = Some(
                "Claude process exited before the background command reported final output"
                    .to_string(),
            );
            entry.state.output_unavailable =
                Some("Background command output unavailable after Claude process exit".to_string());
        }
        emit_background_task_completion(owner, &entry);
    }
}

fn map_background_task_patch_status(raw: &str) -> BackgroundTaskStatus {
    match raw {
        "running" => BackgroundTaskStatus::Running,
        "completed" => BackgroundTaskStatus::Completed,
        "killed" | "stopped" => BackgroundTaskStatus::Stopped,
        "failed" | "error" => BackgroundTaskStatus::Failed,
        other => {
            tracing::warn!("unknown background task_updated patch status: {other:?}");
            BackgroundTaskStatus::Unknown
        }
    }
}

/// Consume a `local_bash` background-command task frame if `value` is
/// one. Like `handle_workflow_task_frame`, this runs pre-gate in
/// `read_claude_stdout_persistent`: a backgrounded command outlives the
/// turn that launched it, and its terminal frames can arrive between
/// turns where the per-turn path would drop them.
///
/// Captured lifecycles (Claude Code 2.1.217):
/// `task_started` (task_type `local_bash`) → on natural completion
/// `task_updated {patch: {status: "completed"}}` then `task_notification
/// {status: "completed", summary}` — or, when the session ends while the
/// command still runs, `task_updated {patch: {status: "killed"}}` then
/// `task_notification {status: "stopped"}`. There are no `task_progress`
/// frames for bash tasks, and `task_updated`/`task_notification` carry
/// no task_type — membership in the registry seeded by `task_started` is
/// the filter. Ownership may be absent on the start frame, so it is resolved
/// from positive tool-request evidence and retried on later lifecycle frames.
/// Once resolved, the owner remains fixed for the detached lifetime.
fn handle_background_bash_task_frame_with_owners(
    value: &Value,
    background_tasks: &mut HashMap<String, BackgroundTaskEntry>,
    root_emitter: &Arc<TurnEmitter>,
    subagent_streams: &HashMap<String, SubAgentStream>,
) -> bool {
    if value.get("type").and_then(Value::as_str) != Some("system") {
        return false;
    }
    let Ok(system) = parse_claude_system_frame(value) else {
        return false;
    };
    let Some(task_id) = system.task_id.as_deref().and_then(normalize_nonempty) else {
        return false;
    };

    match system.event() {
        ClaudeSystemEvent::TaskStarted => {
            if system.task_type.as_deref() != Some("local_bash") {
                return false;
            }
            let Some(tool_use_id) = system.tool_use_id.as_deref().and_then(normalize_nonempty)
            else {
                tracing::warn!("ignoring local_bash task_started without tool_use_id: {value}");
                return true;
            };
            let parent_tool_use_id = background_task_parent_tool_use_id(value).map(str::to_owned);
            let owner = resolve_background_task_owner(
                &tool_use_id,
                parent_tool_use_id.as_deref(),
                root_emitter,
                subagent_streams,
            );
            let tool_name = owner
                .as_deref()
                .and_then(|owner| owner.tool_request_name(&tool_use_id));
            let command = owner
                .as_deref()
                .and_then(|owner| owner.tool_request_command(&tool_use_id));
            let entry = BackgroundTaskEntry {
                tool_use_id,
                tool_name,
                owner,
                parent_tool_use_id,
                state: BackgroundTaskState {
                    task_id: task_id.clone(),
                    description: command,
                    status: BackgroundTaskStatus::Running,
                    summary: None,
                    output_unavailable: None,
                },
                output: None,
                output_path: None,
                terminal_notification_received: false,
            };
            tracing::debug!(
                task_id,
                tool_use_id = entry.tool_use_id,
                parent_tool_use_id = entry.parent_tool_use_id.as_deref().unwrap_or(""),
                owner_resolved = entry.owner.is_some(),
                "registered background Bash task ownership"
            );
            if let Some(owner) = entry.owner.as_deref()
                && entry.tool_name.is_some()
            {
                emit_background_task_snapshot(owner, &entry);
            }
            background_tasks.insert(task_id, entry);
            true
        }
        ClaudeSystemEvent::TaskUpdated => {
            let Some(entry) = background_tasks.get_mut(&task_id) else {
                return false;
            };
            refresh_background_task_owner(value, entry, root_emitter, subagent_streams);
            let patch = system.patch.as_ref();
            if let Some(path) = patch
                .and_then(|patch| patch.output_file.as_ref().or(patch.path.as_ref()))
                .map(String::as_str)
                .and_then(normalize_nonempty)
            {
                entry.output_path = Some(path);
            }
            let Some(status) = patch.and_then(|patch| patch.status.as_deref()) else {
                // A patch with no status (e.g. output-file bookkeeping)
                // changes nothing the tray renders.
                return true;
            };
            let next_status = map_background_task_patch_status(status);
            if entry.state.status != BackgroundTaskStatus::Running {
                if next_status != entry.state.status {
                    tracing::warn!(
                        task_id,
                        current_status = ?entry.state.status,
                        ?next_status,
                        "ignoring background Bash status regression after terminal update"
                    );
                }
                return true;
            }
            entry.state.status = next_status;
            if entry.state.status == BackgroundTaskStatus::Running
                && let Some(owner) = entry.owner.as_ref().map(Arc::clone)
            {
                emit_background_task_snapshot(&owner, entry);
            }
            true
        }
        ClaudeSystemEvent::TaskNotification => {
            let Some(entry) = background_tasks.get_mut(&task_id) else {
                return false;
            };
            refresh_background_task_owner(value, entry, root_emitter, subagent_streams);
            entry.state.status = match system.status.as_deref() {
                Some("completed") => BackgroundTaskStatus::Completed,
                Some("stopped") | Some("killed") => BackgroundTaskStatus::Stopped,
                Some("failed") | Some("error") => BackgroundTaskStatus::Failed,
                other => {
                    tracing::warn!("unknown background task_notification status: {other:?}");
                    BackgroundTaskStatus::Unknown
                }
            };
            entry.state.summary = system.summary.as_deref().and_then(normalize_nonempty);
            let output_path = system
                .output_file
                .as_deref()
                .or(system.path.as_deref())
                .or(entry.output_path.as_deref());
            match capture_background_command_output(output_path) {
                Ok(output) => entry.output = Some(output),
                Err(reason) => entry.state.output_unavailable = Some(reason.to_owned()),
            }
            entry.terminal_notification_received = true;
            let mut completion_emitted = false;
            if let Some(owner) = entry.owner.as_deref()
                && entry.tool_name.is_some()
            {
                emit_background_task_completion(owner, entry);
                completion_emitted = true;
            } else {
                tracing::error!(
                    task_id,
                    tool_use_id = entry.tool_use_id,
                    parent_tool_use_id = entry.parent_tool_use_id.as_deref().unwrap_or(""),
                    "retaining terminal background task frame until ownership resolves"
                );
            }
            if completion_emitted {
                background_tasks.remove(&task_id);
            }
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
    if !parent_emitter.has_known_tool_request(&tool_use_id) {
        return;
    }

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
            parent_tool_name: None,
            name,
            description,
            agent_type: task_type,
            session_id_hint: None,
            execution: SubAgentExecution::Unknown,
        },
    )
    .await;
}

/// Captured Claude Code local-agent lifecycle:
/// `task_started { task_id, tool_use_id, task_type: "local_agent" }` seeds
/// correlation, then `task_progress` and sometimes `task_notification` carry
/// `usage.total_tokens`. Only that numeric field is authoritative here;
/// summaries and status prose are never parsed as accounting data.
fn observe_local_agent_task_usage(
    inner: &ClaudeInner,
    value: &Value,
    task_to_tool_use: &mut HashMap<String, String>,
    streams: &mut HashMap<String, SubAgentStream>,
) {
    if value.get("type").and_then(Value::as_str) != Some("system") {
        return;
    }
    let Ok(system) = parse_claude_system_frame(value) else {
        return;
    };
    let event = system.event();
    let is_notification = event == ClaudeSystemEvent::TaskNotification;
    if event == ClaudeSystemEvent::TaskStarted
        && system.task_type.as_deref() == Some("local_agent")
        && let (Some(task_id), Some(tool_use_id)) =
            (system.task_id.as_deref(), system.tool_use_id.as_deref())
    {
        task_to_tool_use.insert(task_id.to_owned(), tool_use_id.to_owned());
        let mut tasks = inner
            .native_subagent_tasks
            .lock()
            .expect("Claude native subagent task mutex poisoned");
        tasks.remove(tool_use_id);
        tasks.insert(task_id.to_owned());
    }
    if !matches!(
        &event,
        ClaudeSystemEvent::TaskProgress | ClaudeSystemEvent::TaskNotification
    ) {
        return;
    }
    let tool_use_id = system
        .tool_use_id
        .as_deref()
        .or_else(|| {
            system
                .task_id
                .as_deref()
                .and_then(|task_id| task_to_tool_use.get(task_id).map(String::as_str))
        })
        .map(str::to_owned);
    if let Some(tool_use_id) = tool_use_id
        && let Some(total_tokens) = system.usage.and_then(|usage| usage.total_tokens)
        && let Some(stream) = streams.get_mut(&tool_use_id)
        && stream
            .reported_total_tokens
            .is_none_or(|reported| total_tokens > reported)
    {
        stream.reported_total_tokens = Some(total_tokens);
        stream.inner.emitter.total_only_token_usage(total_tokens);
    }
    if is_notification && let Some(task_id) = system.task_id {
        task_to_tool_use.remove(&task_id);
        inner
            .native_subagent_tasks
            .lock()
            .expect("Claude native subagent task mutex poisoned")
            .remove(&task_id);
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
    pending_prompts: &mut HashMap<u64, PendingSubAgentPrompt>,
    pending_spawns: &mut HashMap<String, SubAgentSpawnSpec>,
) {
    fn maybe_update_prompt_from_pending(
        pending: &PendingSubAgentPrompt,
        pending_spawns: &mut HashMap<String, SubAgentSpawnSpec>,
    ) {
        let Ok(parsed) = serde_json::from_str::<Value>(&pending.partial_json) else {
            return;
        };
        let description = extract_spawn_description(Some(&parsed));
        if let Some(spawn) = pending_spawns.get_mut(&pending.tool_use_id) {
            spawn.description = description;
        }
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
            let Some((tool_use_id, _name, _description, _agent_type)) = extract_spawn_info(block)
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
            maybe_update_prompt_from_pending(pending, pending_spawns);
        }
        "content_block_stop" => {
            let Some(index) = content_block_index(&event) else {
                return;
            };
            if let Some(pending) = pending_prompts.remove(&index) {
                maybe_update_prompt_from_pending(&pending, pending_spawns);
            }
        }
        "message_stop" => {
            for pending in pending_prompts.values() {
                maybe_update_prompt_from_pending(pending, pending_spawns);
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
    pending_spawns: &mut HashMap<String, SubAgentSpawnSpec>,
) {
    track_pending_subagent_prompt_event(value, pending_prompts, pending_spawns);

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
            let spec = SubAgentSpawnSpec {
                tool_use_id: tool_use_id.clone(),
                parent_tool_name: Some(block_name.to_owned()),
                name,
                description: description.clone(),
                agent_type,
                session_id_hint: None,
                execution: requested_execution.unwrap_or_default(),
            };
            if parent_emitter.has_known_tool_request(&tool_use_id) {
                ensure_subagent_stream(emitter, parent_emitter, streams, spec).await;
            } else {
                pending_spawns.insert(tool_use_id.clone(), spec);
                continue;
            }
            if let Some(stream) = streams.get_mut(&tool_use_id)
                && let Some(execution) = requested_execution
            {
                stream.execution = execution;
            }
        }
    }
}

async fn flush_pending_subagent_spawns(
    emitter: &dyn SubAgentEmitter,
    parent_emitter: &Arc<TurnEmitter>,
    streams: &mut HashMap<String, SubAgentStream>,
    pending_spawns: &mut HashMap<String, SubAgentSpawnSpec>,
) {
    let ready = pending_spawns
        .keys()
        .filter(|tool_use_id| parent_emitter.has_known_tool_request(tool_use_id))
        .cloned()
        .collect::<Vec<_>>();
    for tool_use_id in ready {
        let Some(spec) = pending_spawns.remove(&tool_use_id) else {
            continue;
        };
        ensure_subagent_stream(emitter, parent_emitter, streams, spec).await;
    }
}

/// Terminal sub-agent data the CLI reports only *outside* the child's
/// correlated stream. The CLI never forwards the child's final assistant
/// turn as a `parent_tool_use_id` frame (verified against 2.1.217): the
/// final text travels solely in the parent's Task `tool_use_result` (and,
/// for background agents, the `task_notification` summary), and the final
/// API call's usage solely in `tool_use_result.usage`. Without carrying
/// these into finalization, the child chat ends on an empty placeholder
/// and the final turn's output tokens go unreported.
#[derive(Default)]
struct SubAgentFinalOutcome {
    text: Option<String>,
    usage: Option<Value>,
}

/// Pull the child's final message and last-call usage from a Task
/// tool_result frame's frame-level `tool_use_result` object.
fn extract_subagent_final_outcome(value: &Value) -> SubAgentFinalOutcome {
    let Some(result) = value.get("tool_use_result") else {
        return SubAgentFinalOutcome::default();
    };
    let text = result
        .get("content")
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|block| block.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .filter(|text| !text.trim().is_empty());
    // Normalize the CLI's raw usage keys (`cache_read_input_tokens`,
    // missing `total_tokens`) into the canonical shape `add_token_usage`
    // and the emitters consume.
    let usage = parse_token_usage(result.get("usage"));
    SubAgentFinalOutcome { text, usage }
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
    // `tool_use_result` is frame-level, so it can describe only one
    // tool_result; hand it to the first matching stream and no other.
    let mut final_outcome = Some(extract_subagent_final_outcome(value));
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
            finalize_subagent_stream(stream, final_outcome.take().unwrap_or_default());
        }
    }
}

/// Flush and close out a sub-agent stream, emitting its final progress stats.
/// `outcome` carries the final assistant text/usage the CLI reports outside
/// the correlated stream (see `SubAgentFinalOutcome`); callers with no such
/// data (process exit) pass `SubAgentFinalOutcome::default()`.
fn finalize_subagent_stream(mut stream: SubAgentStream, outcome: SubAgentFinalOutcome) {
    flush_pending_tool_uses_with_fallback(&mut stream.summary, &mut stream.segment);
    if let Some(text) = outcome
        .text
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())
    {
        // The phase machinery prefers streamed text over `assistant_text`,
        // so if a future CLI starts forwarding the final turn on the
        // correlated stream this stays render-once.
        stream.summary.assistant_text = Some(text.to_owned());
    }
    if let Some(usage) = outcome.usage {
        stream.summary.usage = Some(match stream.summary.usage.take() {
            Some(existing) => add_token_usage(Some(&existing), &usage),
            None => usage,
        });
    }
    if phase_has_pending_output(&stream.summary, &stream.segment) {
        // The child's previous phase closed on its last tool_result, so an
        // injected final message opens a fresh segment: a StreamStart may
        // still be owed before the closing StreamEnd (no-op otherwise).
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
    if stream.inner.emitter.is_stream_open() {
        stream.inner.emit_placeholder_stream_end(
            stream.summary.model.clone(),
            subagent_terminal_usage(&stream.summary),
            None,
        );
    }
    close_terminal_tool_requests(&mut stream.summary, &stream.inner, false);
    // Child liveness is owned by the child stream, not by the parent Task
    // card's completed progress snapshot. Without an explicit idle marker the
    // relay agent can remain Active after its final StreamEnd (or forever when
    // the notification has no renderable text/usage).
    stream.inner.emitter.typing_status_changed(false);
    // Unthrottled final update with the closing stats.
    queue_subagent_progress(&mut stream, true);
    flush_subagent_progress(&mut stream);
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
    if let Some(stream) = streams
        .get_mut(tool_use_id)
        .filter(|stream| stream.execution != SubAgentExecution::Foreground)
    {
        let status = value
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("failed");
        // On a completed agent the notification `summary` is the child's
        // final assistant text — the only carrier of it for background
        // agents. Other statuses describe the failure, not the answer.
        // No usage here: the notification reports only an unsplittable
        // total, and fabricating an input/output split would be dishonest.
        let text = (status == "completed")
            .then(|| value.get("summary").and_then(Value::as_str))
            .flatten()
            .map(str::to_owned);
        stream.pending_terminal = Some((status.to_string(), text));
    }
    finalize_ready_background_subagents(streams);
}

fn finalize_ready_background_subagents(streams: &mut HashMap<String, SubAgentStream>) {
    let ready = streams
        .iter()
        .filter(|(_, stream)| {
            stream.execution == SubAgentExecution::Background
                && stream.pending_terminal.is_some()
                && !stream.inner.emitter.has_pending_background_tools()
        })
        .map(|(id, _)| id.clone())
        .collect::<Vec<_>>();
    for id in ready {
        let mut stream = streams.remove(&id).expect("ready subagent disappeared");
        let (status, text) = stream
            .pending_terminal
            .take()
            .expect("ready subagent lost terminal outcome");
        let succeeded = status == "completed" && !stream.execution_failed;
        let parent_emitter = stream.parent_emitter.clone();
        let parent_tool_use_id = stream.parent_tool_use_id.clone();
        let parent_tool_name = stream.parent_tool_name.clone();
        finalize_subagent_stream(stream, SubAgentFinalOutcome { text, usage: None });
        if succeeded {
            let _ = emit_tool_completion_for_known_request(
                &parent_emitter,
                &parent_tool_use_id,
                &parent_tool_name,
                ToolExecutionOutcome::Succeeded {
                    result: ToolExecutionResult::Other {
                        result: json!({ "status": status }),
                    },
                },
            );
        } else if !parent_emitter.fail_pending_tool(
            &parent_tool_use_id,
            &if status == "completed" {
                "Background agent reported a failed tool execution".to_string()
            } else {
                format!("Background agent ended with status '{status}'")
            },
        ) {
            parent_emitter.backend_error(
                "Claude emitted a tool completion without a pending declared provider-response request",
            );
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

fn consume_claude_stream_value(
    value: &Value,
    summary: &mut ClaudeStdoutSummary,
    segment: &mut SegmentState,
    inner: &ClaudeInner,
    base_message_id: &str,
    current_message_id: &mut String,
) {
    consume_claude_stream_value_with_interrupt(
        value,
        summary,
        segment,
        inner,
        base_message_id,
        current_message_id,
        false,
    );
}

fn consume_claude_stream_value_with_interrupt(
    value: &Value,
    summary: &mut ClaudeStdoutSummary,
    segment: &mut SegmentState,
    inner: &ClaudeInner,
    base_message_id: &str,
    current_message_id: &mut String,
    interrupt_requested: bool,
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
                    if let Some(observation) = claude_live_compaction_observation(
                        value,
                        &system,
                        summary.session_id.as_deref(),
                    ) {
                        inner
                            .emitter
                            .compaction_event(&BackendCompactionEvent::Observed(Box::new(
                                observation,
                            )));
                    }
                }
                // Workflow and local_bash background-command task frames
                // are consumed pre-gate in `read_claude_stdout_persistent`
                // (they keep arriving between turns, when this per-turn
                // path never runs); anything reaching here is some other
                // task event with nothing to render.
                ClaudeSystemEvent::TaskStarted
                | ClaudeSystemEvent::TaskProgress
                | ClaudeSystemEvent::TaskNotification
                | ClaudeSystemEvent::BackgroundTasksChanged
                | ClaudeSystemEvent::TaskUpdated => {
                    let _ = (&system.task_id, &system.status, &system.summary);
                }
                ClaudeSystemEvent::ThinkingTokens => {}
                ClaudeSystemEvent::ApiRetry => {
                    let Some(attempt) = system.attempt else {
                        tracing::warn!(frame = %value, "Claude retry frame omitted attempt");
                        return;
                    };
                    let Some(max_retries) = system.max_retries else {
                        tracing::warn!(frame = %value, "Claude retry frame omitted max_retries");
                        return;
                    };
                    let Some(backoff_ms) = system.retry_delay_ms else {
                        tracing::warn!(frame = %value, "Claude retry frame omitted retry_delay_ms");
                        return;
                    };
                    let error = system
                        .error
                        .filter(|error| !error.trim().is_empty())
                        .or_else(|| system.error_status.map(|status| format!("HTTP {status}")));
                    let Some(error) = error else {
                        tracing::warn!(frame = %value, "Claude retry frame omitted provider error");
                        return;
                    };
                    inner.emitter.retry_attempt(RetryAttemptPayload {
                        attempt,
                        max_retries,
                        error: &error,
                        backoff_ms,
                    });
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
            consume_user_tool_result(value, summary, segment, inner, interrupt_requested);
        }
        "result" => {
            if let Some(session_id) = value.get("session_id").and_then(Value::as_str) {
                summary.session_id = Some(session_id.to_string());
            }
            if let Some(text) = value.get("result").and_then(Value::as_str) {
                summary.result_text = Some(text.to_string());
            }
            if let Some(reasoning) = extract_reasoning_from_result(value) {
                summary.reasoning_bytes = summary
                    .reasoning_bytes
                    .max(u64::try_from(reasoning.len()).unwrap_or(u64::MAX));
                summary.result_reasoning = Some(reasoning);
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
    let has_new_tool_call = next_tool_calls
        .iter()
        .any(|tool_call| !summary.seen_tool_ids.contains(&tool_call.id));

    if has_payload && (!is_duplicate || (segment.awaiting_stream_start && has_new_tool_call)) {
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
        summary.reasoning_bytes = summary
            .reasoning_bytes
            .max(u64::try_from(reasoning.len()).unwrap_or(u64::MAX));
        summary.result_reasoning = Some(reasoning);
        segment.has_content = true;
    }

    if let Some(usage) = next_usage {
        summary.usage = Some(usage);
    }

    for tool_call in next_tool_calls {
        segment
            .pending_tool_uses
            .retain(|_, pending| pending.id != tool_call.id);
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
    interrupt_requested: bool,
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
        Some(&summary.tool_modify_preview_by_id),
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
        if !summary
            .unresolved_tool_requests
            .contains_key(&completion.tool_call_id)
        {
            tracing::debug!(
                tool_call_id = completion.tool_call_id,
                "skipping Claude tool completion without emitted ToolRequest"
            );
            continue;
        }
        // Claude answers an interrupt with a synthetic errored tool_result
        // containing provider control instructions. The turn's authoritative
        // cancellation tail owns this completion; treating the text as Bash
        // stderr fabricates an exit code and leaks internal prose.
        if interrupt_requested && !completion.success {
            continue;
        }
        summary
            .unresolved_tool_requests
            .remove(&completion.tool_call_id);

        // A background Bash tool_result only acknowledges launch. Its process
        // remains live and is completed from task_notification below, where
        // the CLI reports the authoritative terminal state.
        let background_launch = summary
            .tool_call_by_id
            .get(&completion.tool_call_id)
            .is_some_and(|tool| {
                (claude_is_run_command_tool_name(&tool.name) || is_subagent_tool_name(&tool.name))
                    && tool
                        .arguments
                        .get("run_in_background")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
            });
        if background_launch {
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
        reset_phase_state(summary, segment);
        segment.awaiting_stream_start = true;
    } else {
        tracing::warn!(
            stream_open = inner.emitter.is_stream_open(),
            awaiting_stream_start = segment.awaiting_stream_start,
            has_content = segment.has_content,
            pending_tool_uses = segment.pending_tool_uses.len(),
            has_streamed_text = !summary.streamed_text.is_empty(),
            has_streamed_reasoning = !summary.streamed_reasoning.is_empty(),
            has_assistant_text = summary.assistant_text.is_some(),
            has_result_text = summary.result_text.is_some(),
            has_result_reasoning = summary.result_reasoning.is_some(),
            "Claude phase close found pending state without an emittable payload; retaining the current stream"
        );
    }
}

fn emit_tool_request_with_tracking(
    summary: &mut ClaudeStdoutSummary,
    inner: &ClaudeInner,
    tool_call: &ClaudeToolCall,
) {
    if inner.emit_tool_request(tool_call) {
        summary
            .unresolved_tool_requests
            .insert(tool_call.id.clone(), tool_call.name.clone());
    }
}

fn auto_close_unresolved_tool_requests(
    summary: &mut ClaudeStdoutSummary,
    inner: &ClaudeInner,
    message: &str,
) {
    let unresolved = std::mem::take(&mut summary.unresolved_tool_requests);
    for (tool_call_id, tool_name) in unresolved {
        eprintln!("TYDE CLAUDE AUTO CLOSE id={tool_call_id} name={tool_name} reason={message:?}");
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

fn close_terminal_tool_requests(
    summary: &mut ClaudeStdoutSummary,
    inner: &ClaudeInner,
    cancelled: bool,
) {
    if !cancelled {
        auto_close_unresolved_tool_requests(
            summary,
            inner,
            "Claude ended the turn before returning a result for this streamed tool request.",
        );
        return;
    }

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
                "kind": "Cancelled",
                "message": "Cancelled by user",
            }),
            None,
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
        finish_pending_tool_use(summary, segment, index);
    }
}

fn flush_pending_tool_uses_with_fallback(
    summary: &mut ClaudeStdoutSummary,
    segment: &mut SegmentState,
) {
    flush_pending_tool_uses(summary, segment);
    let pending = std::mem::take(&mut segment.pending_tool_uses);
    for (_, pending) in pending {
        register_tool_call_for_phase(
            summary,
            segment,
            ClaudeToolCall {
                id: pending.id,
                name: pending.name,
                arguments: pending.arguments,
            },
        );
    }
}

fn finish_pending_tool_use(
    summary: &mut ClaudeStdoutSummary,
    segment: &mut SegmentState,
    index: u64,
) {
    maybe_emit_pending_tool_use(summary, segment, index);
    if segment
        .pending_tool_uses
        .get(&index)
        .is_some_and(|pending| pending.request_emitted)
    {
        segment.pending_tool_uses.remove(&index);
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

    if segment.awaiting_stream_start
        && matches!(event_type, "content_block_start" | "content_block_delta")
    {
        tracing::info!(
            event_type,
            provider_message_id = ?segment.current_claude_message_id,
            "Claude content block started a response phase without message_start"
        );
        let model = summary.model.clone();
        maybe_emit_next_stream_start(
            summary,
            segment,
            inner,
            base_message_id,
            current_message_id,
            model,
        );
    }

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
            finish_pending_tool_use(summary, segment, index);
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

fn claude_is_task_create_tool_name(tool_name: &str) -> bool {
    normalize_tool_name(tool_name) == "taskcreate"
}

fn claude_is_task_update_tool_name(tool_name: &str) -> bool {
    normalize_tool_name(tool_name) == "taskupdate"
}

#[derive(Default)]
struct ClaudeTaskTracker {
    tasks: BTreeMap<u64, protocol::Task>,
    provider_ids: HashMap<u64, u64>,
    pending: HashMap<String, ClaudePendingTaskCall>,
    next_local_id: u64,
}

enum ClaudePendingTaskCall {
    Create { local_id: Option<u64> },
    Update { provider_id: u64, arguments: Value },
}

impl ClaudeTaskTracker {
    fn observe_request(&mut self, tool_call: &ClaudeToolCall) -> Option<protocol::TaskList> {
        if claude_is_todo_write_tool_name(&tool_call.name) {
            let tasks = claude_task_update_from_todo_write(&tool_call.arguments)?;
            self.tasks = tasks
                .tasks
                .iter()
                .cloned()
                .map(|task| (task.id, task))
                .collect();
            self.provider_ids.clear();
            self.pending.clear();
            self.next_local_id = self
                .tasks
                .keys()
                .next_back()
                .copied()
                .unwrap_or(0)
                .saturating_add(1);
            return Some(tasks);
        }

        if claude_is_task_create_tool_name(&tool_call.name) {
            let local_id = task_description(&tool_call.arguments).map(|description| {
                let local_id = self.next_local_id.max(1);
                self.next_local_id = local_id.saturating_add(1);
                self.tasks.insert(
                    local_id,
                    protocol::Task {
                        id: local_id,
                        description,
                        status: protocol::TaskStatus::Pending,
                    },
                );
                local_id
            });
            self.pending.insert(
                tool_call.id.clone(),
                ClaudePendingTaskCall::Create { local_id },
            );
            return local_id.map(|_| self.snapshot());
        }

        if claude_is_task_update_tool_name(&tool_call.name) {
            let provider_id = task_id_from_value(&tool_call.arguments)?;
            self.pending.insert(
                tool_call.id.clone(),
                ClaudePendingTaskCall::Update {
                    provider_id,
                    arguments: tool_call.arguments.clone(),
                },
            );
        }
        None
    }

    fn observe_completion(
        &mut self,
        tool_call_id: &str,
        tool_name: &str,
        result: &Value,
    ) -> Option<protocol::TaskList> {
        let pending = self.pending.remove(tool_call_id)?;
        let before = serde_json::to_value(self.snapshot()).ok()?;
        match pending {
            ClaudePendingTaskCall::Create { local_id }
                if claude_is_task_create_tool_name(tool_name) =>
            {
                let local_id = match local_id {
                    Some(local_id) => local_id,
                    None => {
                        let description = task_description_from_result(result)?;
                        let local_id = self.next_local_id.max(1);
                        self.next_local_id = local_id.saturating_add(1);
                        self.tasks.insert(
                            local_id,
                            protocol::Task {
                                id: local_id,
                                description,
                                status: protocol::TaskStatus::Pending,
                            },
                        );
                        local_id
                    }
                };
                if let Some(provider_id) = task_id_from_value(result) {
                    self.provider_ids.insert(provider_id, local_id);
                }
            }
            ClaudePendingTaskCall::Update {
                provider_id,
                arguments,
            } if claude_is_task_update_tool_name(tool_name) => {
                let local_id = self
                    .provider_ids
                    .get(&provider_id)
                    .copied()
                    .or_else(|| self.tasks.contains_key(&provider_id).then_some(provider_id))?;
                let task = self.tasks.get_mut(&local_id)?;
                if let Some(description) = task_description(&arguments) {
                    task.description = description;
                }
                if let Some(status) = arguments
                    .get("status")
                    .and_then(Value::as_str)
                    .and_then(task_status)
                {
                    task.status = status;
                }
            }
            _ => return None,
        }
        let snapshot = self.snapshot();
        (serde_json::to_value(&snapshot).ok()? != before).then_some(snapshot)
    }

    fn snapshot(&self) -> protocol::TaskList {
        protocol::TaskList {
            title: String::new(),
            tasks: self.tasks.values().cloned().collect(),
        }
    }
}

fn task_description(value: &Value) -> Option<String> {
    ["subject", "description", "content", "activeForm"]
        .iter()
        .find_map(|key| value.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn task_description_from_result(value: &Value) -> Option<String> {
    if let Some(description) = task_description(value) {
        return Some(description);
    }
    let text = value.as_str()?.trim();
    text.split_once(':')
        .map(|(_, description)| description.trim())
        .filter(|description| !description.is_empty())
        .map(str::to_string)
}

fn task_id_from_value(value: &Value) -> Option<u64> {
    for key in ["taskId", "task_id", "id"] {
        if let Some(id) = value.get(key).and_then(value_as_u64) {
            return Some(id);
        }
    }
    if let Some(text) = value.as_str() {
        if let Some(after_hash) = text.split('#').nth(1) {
            let digits = after_hash
                .chars()
                .take_while(|character| character.is_ascii_digit())
                .collect::<String>();
            if let Ok(id) = digits.parse() {
                return Some(id);
            }
        }
        return text.trim().parse().ok();
    }
    value
        .as_object()
        .and_then(|object| object.values().find_map(task_id_from_value))
}

fn value_as_u64(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
}

fn task_status(value: &str) -> Option<protocol::TaskStatus> {
    match value.trim().to_ascii_lowercase().as_str() {
        "pending" => Some(protocol::TaskStatus::Pending),
        "in_progress" | "inprogress" | "active" => Some(protocol::TaskStatus::InProgress),
        "completed" | "complete" | "done" => Some(protocol::TaskStatus::Completed),
        "failed" | "cancelled" | "canceled" | "deleted" => Some(protocol::TaskStatus::Failed),
        _ => None,
    }
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
            let before = std::fs::read_to_string(&file_path).unwrap_or_default();
            (before, after)
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

    let parsed = options
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
        .collect::<Vec<_>>();
    if parsed.len() == 2
        && parsed[0].label == CLAUDE_FREE_TEXT_SENTINEL
        && parsed[1].label == CLAUDE_FREE_TEXT_OTHER
    {
        Vec::new()
    } else {
        parsed
    }
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
    if is_subagent_tool_name(tool_name) {
        let prompt = claude_argument_string(arguments, &["prompt"])
            .or_else(|| claude_argument_string(arguments, &["description"]));
        let name = claude_argument_string(arguments, &["name"])
            .or_else(|| claude_argument_string(arguments, &["description"]));
        let execution_mode =
            if claude_argument_bool(arguments, &["run_in_background"]).unwrap_or(false) {
                protocol::AgentExecutionMode::Background
            } else {
                protocol::AgentExecutionMode::Foreground
            };
        return serde_json::to_value(protocol::ToolRequestType::AgentSpawn {
            prompt,
            name,
            execution_mode,
        })
        .expect("serialize Claude agent spawn request");
    }

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
        let file_paths = claude_argument_file_paths(arguments);
        if !file_paths.is_empty() {
            return json!({
                "kind": "ReadFiles",
                "file_paths": file_paths,
            });
        }
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
        "args": arguments,
    })
}

fn claude_public_tool_result(tool_name: &str, success: bool, tool_result: Value) -> Value {
    if !is_subagent_tool_name(tool_name) {
        return tool_result;
    }

    serde_json::to_value(protocol::ToolExecutionResult::Other {
        result: json!({
            "status": if success { "completed" } else { "failed" },
        }),
    })
    .expect("serialize Claude agent result")
}

fn claude_tool_execution_outcome(
    success: bool,
    tool_result: Value,
    error: Option<String>,
) -> ToolExecutionOutcome {
    if success {
        let result = serde_json::from_value::<ToolExecutionResult>(tool_result.clone()).unwrap_or(
            ToolExecutionResult::Other {
                result: tool_result,
            },
        );
        return ToolExecutionOutcome::Succeeded { result };
    }
    if tool_result.get("kind").and_then(Value::as_str) == Some("Cancelled") {
        return ToolExecutionOutcome::Cancelled {
            message: tool_result
                .get("message")
                .and_then(Value::as_str)
                .and_then(normalize_nonempty)
                .unwrap_or_else(|| "Tool execution was cancelled".to_owned()),
        };
    }
    let details = tool_result
        .get("detailed_message")
        .and_then(Value::as_str)
        .and_then(normalize_nonempty)
        .or_else(|| (!tool_result.is_null()).then(|| tool_result.to_string()));
    ToolExecutionOutcome::Failed {
        message: error
            .and_then(|message| normalize_nonempty(&message))
            .or_else(|| {
                tool_result
                    .get("short_message")
                    .and_then(Value::as_str)
                    .and_then(normalize_nonempty)
            })
            .unwrap_or_else(|| "Tool execution failed".to_owned()),
        details,
        normalization_failure: None,
    }
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
        if let Some(observation) = claude_replay_compaction_observation(&value) {
            flush_unresolved_replay_tool_requests(
                &mut restored,
                &mut pending_tool_requests,
                &mut auto_closed_tool_requests,
            );
            restored.push(ClaudeHistoryReplayItem::Compaction(observation));
            last_emitted_assistant_message_id = None;
            continue;
        }
        if claude_replay_row_is_internal_compaction_bookkeeping(&value) {
            continue;
        }
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
        } else if is_tool_only_assistant_continuation {
            let Some(previous) = restored.iter_mut().rev().find_map(|item| match item {
                ClaudeHistoryReplayItem::Message(message) => Some(message),
                _ => None,
            }) else {
                tracing::error!("Claude replay tool continuation had no owning assistant message");
                debug_assert!(
                    false,
                    "Claude replay tool continuation had no owning assistant message"
                );
                continue;
            };
            let Some(previous_tool_calls) =
                previous.get_mut("tool_calls").and_then(Value::as_array_mut)
            else {
                tracing::error!("Claude replay assistant message had no tool_calls array");
                debug_assert!(
                    false,
                    "Claude replay assistant message had no tool_calls array"
                );
                continue;
            };
            previous_tool_calls.extend(message_tool_calls.clone());
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
            None,
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

fn claude_replay_compaction_observation(value: &Value) -> Option<BackendObservedCompaction> {
    if value.get("type").and_then(Value::as_str) != Some("system")
        || value.get("subtype").and_then(Value::as_str) != Some("compact_boundary")
    {
        return None;
    }
    let boundary_uuid = value
        .get("uuid")
        .and_then(Value::as_str)
        .and_then(normalize_nonempty)?;
    let session_id = value
        .get("sessionId")
        .or_else(|| value.get("session_id"))
        .and_then(Value::as_str)
        .and_then(normalize_nonempty)?;
    let metadata = value
        .get("compactMetadata")
        .or_else(|| value.get("compact_metadata"))
        .filter(|value| value.is_object());
    let trigger = metadata
        .and_then(|metadata| metadata.get("trigger"))
        .and_then(Value::as_str)
        .unwrap_or("auto");
    let trigger = if trigger == "manual" {
        CompactionTrigger::BackendObservedManual
    } else {
        CompactionTrigger::BackendAutomatic
    };
    let metrics = claude_compaction_metrics(metadata);
    let user_focus = metadata
        .and_then(|metadata| {
            metadata
                .get("userContext")
                .or_else(|| metadata.get("user_context"))
        })
        .and_then(Value::as_str)
        .and_then(normalize_nonempty)
        .map(|text| BackendCompactionUserFocus {
            text,
            provenance: BackendCompactionUserFocusProvenance::ProviderEcho,
        });
    Some(BackendObservedCompaction {
        observation_id: super::compaction::stable_observation_id(
            "claude",
            &session_id,
            &boundary_uuid,
        ),
        trigger,
        method: if trigger == CompactionTrigger::BackendAutomatic {
            CompactionMethod::BackendAutomatic
        } else {
            CompactionMethod::NativeTextCommand
        },
        provider_session_id: Some(SessionId(session_id)),
        metrics,
        source: BackendCompactionObservationSource::ClaudeBoundary { boundary_uuid },
        user_focus,
    })
}

fn claude_compaction_metrics(metadata: Option<&Value>) -> CompactionMetrics {
    let get_u64 = |camel: &str, snake: &str| {
        metadata
            .and_then(|metadata| metadata.get(camel).or_else(|| metadata.get(snake)))
            .and_then(Value::as_u64)
    };
    CompactionMetrics {
        before_tokens: get_u64("preTokens", "pre_tokens"),
        after_tokens: get_u64("postTokens", "post_tokens"),
        before_messages: get_u64("beforeMessages", "before_messages"),
        after_messages: get_u64("afterMessages", "after_messages"),
        messages_summarized: get_u64("messagesSummarized", "messages_summarized"),
        cumulative_dropped_tokens: get_u64("cumulativeDroppedTokens", "cumulative_dropped_tokens"),
        duration_ms: get_u64("durationMs", "duration_ms"),
        precomputed: metadata
            .and_then(|metadata| {
                metadata
                    .get("precomputed")
                    .or_else(|| metadata.get("pre_computed"))
            })
            .and_then(Value::as_bool),
    }
}

fn claude_replay_row_is_internal_compaction_bookkeeping(value: &Value) -> bool {
    if value
        .get("isVisibleInTranscriptOnly")
        .and_then(Value::as_bool)
        == Some(true)
        || value.get("isCompactSummary").and_then(Value::as_bool) == Some(true)
        || value.get("isMeta").and_then(Value::as_bool) == Some(true)
    {
        return true;
    }
    if value.get("type").and_then(Value::as_str) != Some("user") {
        return false;
    }
    let Some(message) = value.get("message").filter(|message| message.is_object()) else {
        return false;
    };
    let Some(text) = extract_text_from_message(message) else {
        return false;
    };
    let text = text.trim_start();
    text.starts_with("<command-name>")
        || text.starts_with("<local-command-caveat>")
        || text.starts_with("<local-command-stdout>")
        || text.starts_with("<local-command-stderr>")
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
    tool_modify_preview_by_id: Option<&HashMap<String, ClaudeModifyPreview>>,
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
        let modify_preview = tool_modify_preview_by_id
            .and_then(|previews| previews.get(&tool_call_id))
            .cloned()
            .or_else(|| {
                tool_call_by_id.get(&tool_call_id).and_then(|tool_call| {
                    claude_modify_preview(&tool_call.name, &tool_call.arguments)
                })
            });
        let result_text = extract_tool_result_content(block);
        let is_run_command = claude_is_run_command_tool_name(&tool_name);
        let is_read_tool = claude_is_read_tool_name(&tool_name);
        let is_error = block
            .get("is_error")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        if tool_name.starts_with("mcp__") {
            let normalized = normalize_mcp_call_tool_result(block);
            events.push(ClaudeReplayToolExecution {
                tool_call_id,
                tool_name,
                success: normalized.success,
                tool_result: normalized.tool_result,
                error: normalized.error,
            });
            continue;
        }

        if is_error {
            // AskUserQuestion, ExitPlanMode, and EnterPlanMode return is_error in
            // --print mode because they need interactive input. This is expected —
            // treat them as successful end-of-turn signals.
            if claude_is_user_input_tool_name(&tool_name)
                && normalize_tool_name(&tool_name) != "askuserquestion"
            {
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
            let file_paths = tool_call_by_id
                .get(&tool_call_id)
                .map(|tool_call| claude_argument_file_paths(&tool_call.arguments))
                .unwrap_or_default();
            let tool_result = if let [path] = file_paths.as_slice() {
                eprintln!(
                    "TYDE CLAUDE READ RESULT path={path:?} result_text={result_text:?} result_bytes={}",
                    result_text.len()
                );
                let bytes = std::fs::metadata(path)
                    .map(|metadata| metadata.len())
                    .unwrap_or(result_text.len() as u64);
                json!({
                    "kind": "ReadFiles",
                    "files": [{
                        "path": path,
                        "bytes": bytes
                    }],
                })
            } else {
                claude_replay_other_tool_result(block, &result_text)
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

        events.push(ClaudeReplayToolExecution {
            tool_call_id,
            tool_name,
            success: true,
            tool_result: claude_replay_other_tool_result(block, &result_text),
            error: None,
        });
    }

    events
}

fn claude_replay_other_tool_result(block: &Value, result_text: &str) -> Value {
    let has_text_content = match block.get("content") {
        Some(Value::String(_)) => true,
        Some(Value::Array(parts)) => parts.iter().any(|part| {
            part.as_str()
                .or_else(|| part.get("text").and_then(Value::as_str))
                .is_some_and(|text| !text.is_empty())
        }),
        _ => false,
    };
    let result = if result_text.trim().is_empty() || !has_text_content {
        block.clone()
    } else {
        Value::String(result_text.to_string())
    };
    json!({
        "kind": "Other",
        "result": result,
    })
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
    AgentInput, BackendKind, ChatEvent, ChatMessage, CompactionMethod, CompactionMetrics,
    CompactionStage, CompactionTrigger, MessageSender, SelectOption, SessionSettingField,
    SessionSettingFieldType, SessionSettingValue, SessionSettingsSchema, SpawnCostHint,
};

use super::{
    Backend, BackendCompactionCapability, BackendCompactionCapabilityEvidence,
    BackendCompactionDeferredReason, BackendCompactionDispatchState, BackendCompactionEvent,
    BackendCompactionFailure, BackendCompactionFailureKind, BackendCompactionMechanism,
    BackendCompactionMutationState, BackendCompactionNotDispatchedReason,
    BackendCompactionObservationSource, BackendCompactionProgress, BackendCompactionRequest,
    BackendCompactionResult, BackendCompactionStart, BackendCompactionSuccess,
    BackendCompactionTerminalEvidence, BackendCompactionUnavailableReason,
    BackendCompactionUnknownReason, BackendCompactionUserFocus,
    BackendCompactionUserFocusProvenance, BackendEvent, BackendObservedCompaction, BackendSession,
    BackendSpawnConfig, BackendStartupError, EventStream, PostCompactionTokenCount,
    protocol_images_to_attachments, resolve_settings as resolve_backend_settings,
    session_settings_to_json,
};

type ClaudeReadyTx = Arc<Mutex<Option<oneshot::Sender<Result<(), String>>>>>;

fn claude_permission_mode_for_access_mode(access_mode: BackendAccessMode) -> &'static str {
    match access_mode {
        BackendAccessMode::Unrestricted | BackendAccessMode::ReadOnly => {
            CLAUDE_DEFAULT_PERMISSION_MODE
        }
    }
}

/// Minimal Backend-trait handle for the Claude CLI.
///
/// Holds an `mpsc::UnboundedSender<AgentInput>` that the spawned task reads from;
/// the task writes stdin of the child process accordingly.
pub struct ClaudeBackend {
    input_tx: mpsc::UnboundedSender<AgentInput>,
    interrupt_tx: mpsc::UnboundedSender<ClaudeInterrupt>,
    startup_cancel_tx: Option<oneshot::Sender<()>>,
    session_id: Arc<std::sync::Mutex<Option<SessionId>>>,
    subagent_emitter_tx: watch::Sender<Option<Arc<dyn SubAgentEmitter>>>,
    /// Direct handle to the live session, populated once the spawn task has
    /// created it. `send_with_outcome` uses it to run turn admission
    /// synchronously so a busy backend can hand the message back instead of
    /// dropping it in the input-pump task.
    command_handle: Arc<StdMutex<Option<ClaudeCommandHandle>>>,
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
        let (events_tx, events_rx) = mpsc::unbounded_channel::<BackendEvent>();
        let session_id = Arc::new(std::sync::Mutex::new(None));
        let session_id_task = Arc::clone(&session_id);
        let (subagent_emitter_tx, mut subagent_emitter_rx) = watch::channel(initial_emitter);
        let (ready_tx, ready_rx) = oneshot::channel::<Result<(), String>>();
        let (startup_cancel_tx, mut startup_cancel_rx) = oneshot::channel();
        let mut startup_cancel_guard = ClaudeDetachedStartupCancelGuard(Some(startup_cancel_tx));
        let command_handle: Arc<StdMutex<Option<ClaudeCommandHandle>>> =
            Arc::new(StdMutex::new(None));
        let command_handle_task = Arc::clone(&command_handle);

        tokio::spawn(async move {
            // The probe must run where the session will run, so resolve the
            // workspace the same way `spawn_with_mode` does.
            let probe_workspace_root = match pick_workspace_root(&workspace_roots) {
                Ok(root) => root,
                Err(err) => {
                    let _ = ready_tx.send(Err(err));
                    return;
                }
            };
            // Materialize before the process starts, so a capability miss or a
            // failed materialization is one notice at startup rather than a
            // skill that turns up missing mid-turn. It never fails the spawn:
            // see `claude_prepare_skills`.
            // `ClaudeBackend` spawns the CLI locally; the low-level session is
            // the only layer that takes an ssh host, and it is given none here.
            let (skills, skill_steering) =
                claude_prepare_skills(&config, None, &probe_workspace_root).await;
            let steering = match claude_steering_content(&config, skill_steering) {
                Ok(steering) => steering,
                Err(err) => {
                    tracing::error!("Failed to prepare Claude skills: {err}");
                    let _ = ready_tx.send(Err(err));
                    return;
                }
            };
            let steering_content = steering;
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
                        // A fork is a new CLI process, so it needs its own
                        // handle on the same root the session materialized.
                        skills,
                    },
                )
                .await
            } else {
                ClaudeSession::spawn_with_skills(
                    &workspace_roots,
                    None,
                    &config.startup_mcp_servers,
                    steering_content.as_deref(),
                    agent_identity.as_ref(),
                    config.resolved_spawn_config.tool_policy.clone(),
                    config.resolved_spawn_config.access_mode,
                    skills,
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
            session
                .seed_installed_provider_version(config.provider_version.clone())
                .await;

            let handle = session.command_handle();
            *command_handle_task
                .lock()
                .expect("Claude command handle slot poisoned") = Some(handle.clone());
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
        let startup_cancel_tx = startup_cancel_guard.disarm();

        Ok((
            Self {
                input_tx,
                interrupt_tx,
                startup_cancel_tx: Some(startup_cancel_tx),
                session_id,
                subagent_emitter_tx,
                command_handle,
            },
            EventStream::new_backend(events_rx),
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

/// How this session tells the model about skills.
///
/// Locally the model discovers them itself and the overlay is guidance only —
/// no names for the Default agent, compact names for an explicit selection, and
/// never a body. Over SSH there is no discovery seam, so the bodies are the
/// message.
#[derive(Debug)]
enum ClaudeSkillSteering {
    /// No skill was exposed. Emits nothing.
    None,
    /// Local session with a materialized plugin. Carries the skills that
    /// actually materialized, so the overlay can never name one that did not.
    Native(SkillSelection, Vec<PreparedSkill>),
}

fn claude_steering_content(
    config: &BackendSpawnConfig,
    skills: ClaudeSkillSteering,
) -> Result<Option<String>, String> {
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
    let selected = &config.resolved_spawn_config.skills;
    if !selected.is_empty() {
        match skills {
            ClaudeSkillSteering::None => {}
            ClaudeSkillSteering::Native(selection, prepared) => {
                if !prepared.is_empty() {
                    sections.push(native_skill_overlay(selection, &prepared));
                }
            }
        }
    }
    Ok((!sections.is_empty()).then(|| sections.join("\n\n")))
}

fn spawn_claude_subagent_event_bridge(
    mut raw_rx: mpsc::UnboundedReceiver<Value>,
    event_tx: mpsc::UnboundedSender<ChatEvent>,
    model_usage_tx: mpsc::UnboundedSender<protocol::ModelRequestTokenUsage>,
    total_usage_tx: mpsc::UnboundedSender<u64>,
) {
    tokio::spawn(async move {
        while let Some(raw) = raw_rx.recv().await {
            if raw.get("kind").and_then(Value::as_str) == Some("ModelRequestTokenUsage")
                && let Some(data) = raw.get("data")
                && let Ok(usage) = serde_json::from_value(data.clone())
            {
                if model_usage_tx.send(usage).is_err() {
                    break;
                }
                continue;
            }
            if raw.get("kind").and_then(Value::as_str) == Some("TotalOnlyTokenUsage")
                && let Some(total_tokens) =
                    raw.pointer("/data/total_tokens").and_then(Value::as_u64)
            {
                if total_usage_tx.send(total_tokens).is_err() {
                    break;
                }
                continue;
            }
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
    events_tx: &mpsc::UnboundedSender<BackendEvent>,
    session_id_sink: &Arc<std::sync::Mutex<Option<SessionId>>>,
    ready_tx: Option<&ClaudeReadyTx>,
) -> bool {
    if let Ok(event) = serde_json::from_value::<ChatEvent>(raw.clone()) {
        return events_tx.send(BackendEvent::Chat(event)).is_ok();
    }

    match raw.get("kind").and_then(Value::as_str).unwrap_or_default() {
        "BackendCompaction" => {
            let Some(data) = raw.get("data") else {
                return true;
            };
            if let Ok(event) = serde_json::from_value::<BackendCompactionEvent>(data.clone()) {
                return events_tx.send(BackendEvent::Compaction(event)).is_ok();
            }
        }
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
                .send(BackendEvent::Chat(backend_error_message(message.clone())))
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

fn claude_compaction_capability(
    compact_command_advertised: Option<bool>,
    provider_version: Option<&str>,
) -> BackendCompactionCapability {
    let provider_version = provider_version.and_then(normalize_nonempty);
    let evidence = BackendCompactionCapabilityEvidence::ClaudeInitializeCommand {
        name: "compact".to_owned(),
    };
    match compact_command_advertised {
        None => BackendCompactionCapability::unknown(
            BackendCompactionUnknownReason::ProcessNotInitialized,
            provider_version,
            BackendCompactionCapabilityEvidence::None,
        ),
        Some(false) => BackendCompactionCapability::context_unavailable_with_metadata(
            BackendCompactionUnavailableReason::ProviderDisabledCommand,
            provider_version,
            evidence,
        ),
        Some(true) => BackendCompactionCapability::native(
            BackendCompactionMechanism::InterceptedTextCommand,
            provider_version,
            evidence,
        ),
    }
}

fn sanitize_claude_compaction_focus(value: &str) -> Result<String, ()> {
    let sanitized = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if sanitized.len() > 4_096 {
        return Err(());
    }
    Ok(sanitized)
}

fn claude_compaction_focus(request: &BackendCompactionRequest) -> Result<Option<String>, ()> {
    let Some(focus) = request.focus.as_deref() else {
        return Ok(None);
    };
    let focus = sanitize_claude_compaction_focus(focus)?;
    if focus.is_empty() {
        return Ok(None);
    }
    Ok(Some(focus))
}

fn dispatch_uncertain_claude_result(
    operation_id: protocol::CompactionOperationId,
    error: String,
) -> BackendCompactionResult {
    BackendCompactionResult {
        operation_id,
        dispatch: BackendCompactionDispatchState::MayHaveReachedProvider,
        mutation: BackendCompactionMutationState::MayHaveMutated,
        outcome: Err(BackendCompactionFailure {
            kind: BackendCompactionFailureKind::TransportClosed,
            message: error,
        }),
        provider_session_id: None,
        metrics: CompactionMetrics::default(),
        post_context_tokens: PostCompactionTokenCount::Unknown,
        evidence: BackendCompactionTerminalEvidence::DispatchUncertain,
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

impl Backend for ClaudeBackend {
    fn capabilities() -> tyde_agent_adapter::BackendCapabilities {
        [
            tyde_agent_adapter::BackendCapability::ResumeSession,
            tyde_agent_adapter::BackendCapability::ForkSession,
            tyde_agent_adapter::BackendCapability::ImageInput,
            tyde_agent_adapter::BackendCapability::Interrupt,
            tyde_agent_adapter::BackendCapability::SessionSettings,
            tyde_agent_adapter::BackendCapability::StartupMcpServers,
            tyde_agent_adapter::BackendCapability::AgentControlTools,
            tyde_agent_adapter::BackendCapability::TurnUsageReported,
            tyde_agent_adapter::BackendCapability::CompactionReported,
            tyde_agent_adapter::BackendCapability::Subagents,
            tyde_agent_adapter::BackendCapability::ForegroundSubagents,
            tyde_agent_adapter::BackendCapability::BackgroundSubagents,
            tyde_agent_adapter::BackendCapability::BackgroundTasks,
            tyde_agent_adapter::BackendCapability::AgentInitiatedTurns,
            tyde_agent_adapter::BackendCapability::ReasoningDeltas,
            tyde_agent_adapter::BackendCapability::TaskUpdates,
            tyde_agent_adapter::BackendCapability::WorkflowProgress,
            tyde_agent_adapter::BackendCapability::UserQuestionRequests,
            tyde_agent_adapter::BackendCapability::PlanApprovalRequests,
            tyde_agent_adapter::BackendCapability::WorkspaceInstructions,
            tyde_agent_adapter::BackendCapability::Customization,
            tyde_agent_adapter::BackendCapability::GenericModifyFile,
            tyde_agent_adapter::BackendCapability::GenericReadFiles,
            tyde_agent_adapter::BackendCapability::GenericOtherTool,
            tyde_agent_adapter::BackendCapability::CapacityTelemetry,
            tyde_agent_adapter::BackendCapability::RetryTelemetry,
        ]
        .into()
    }

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
        let (events_tx, events_rx) = mpsc::unbounded_channel::<BackendEvent>();
        let (resume_replay_complete_tx, resume_replay_complete_rx) =
            tokio::sync::oneshot::channel();
        let (subagent_emitter_tx, mut subagent_emitter_rx) =
            watch::channel::<Option<Arc<dyn SubAgentEmitter>>>(None);

        let session_id = session_id.0;
        let backend_session_id =
            Arc::new(std::sync::Mutex::new(Some(SessionId(session_id.clone()))));
        let backend_session_id_task = Arc::clone(&backend_session_id);

        // A resumed session gets its own plugin root: the previous session's
        // was unlinked when it shut down.
        let probe_workspace_root = pick_workspace_root(&workspace_roots)?;
        let (skills, skill_steering) =
            claude_prepare_skills(&config, None, &probe_workspace_root).await;
        let steering = claude_steering_content(&config, skill_steering)?;
        let steering_content = steering;
        let agent_identity = claude_agent_identity(&config);
        let (session, mut raw_events) = ClaudeSession::spawn_with_skills(
            &workspace_roots,
            None,
            &config.startup_mcp_servers,
            steering_content.as_deref(),
            agent_identity.as_ref(),
            config.resolved_spawn_config.tool_policy.clone(),
            config.resolved_spawn_config.access_mode,
            skills,
        )
        .await
        .map_err(|err| format!("Failed to spawn Claude resume session: {err}"))?;
        session
            .seed_installed_provider_version(config.provider_version.clone())
            .await;
        let mut startup_guard = ClaudeResumeStartupGuard::new(session.clone());

        let handle = session.command_handle();
        let backend_command_handle = handle.clone();
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

        loop {
            match tokio::time::timeout(RESUME_REPLAY_SETTLE_QUIET, raw_events.recv()).await {
                Ok(Some(raw)) => {
                    if !forward_claude_backend_event(
                        raw,
                        &events_tx,
                        &backend_session_id_task,
                        None,
                    )
                    .await
                    {
                        startup_guard.disarm();
                        session.shutdown().await;
                        return Err("Claude resume event stream closed during replay".to_string());
                    }
                }
                Ok(None) => {
                    startup_guard.disarm();
                    session.shutdown().await;
                    return Err("Claude resume event stream closed during replay".to_string());
                }
                Err(_) => {
                    if session
                        .inner
                        .await_active_turn_quiesced(RESUME_REPLAY_TURN_QUIESCE)
                        .await
                    {
                        break;
                    }
                }
            }
        }
        eprintln!(
            "TYDE CLAUDE RESUME REPLAY SETTLED session={}",
            backend_session_id_task
                .lock()
                .expect("Claude backend session id mutex poisoned")
                .as_ref()
                .map_or("<none>", |session_id| session_id.0.as_str()),
        );
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
                startup_cancel_tx: None,
                session_id: backend_session_id,
                subagent_emitter_tx,
                command_handle: Arc::new(StdMutex::new(Some(backend_command_handle))),
            },
            EventStream::new_backend_with_resume_replay_barrier(
                events_rx,
                resume_replay_complete_rx,
            ),
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

    fn compaction_capability(&self) -> BackendCompactionCapability {
        self.command_handle
            .lock()
            .expect("Claude command handle slot poisoned")
            .as_ref()
            .map(ClaudeCommandHandle::compaction_capability)
            .unwrap_or_else(|| {
                BackendCompactionCapability::unknown(
                    BackendCompactionUnknownReason::ProcessNotInitialized,
                    None,
                    BackendCompactionCapabilityEvidence::None,
                )
            })
    }

    async fn begin_compaction(&self, request: BackendCompactionRequest) -> BackendCompactionStart {
        let handle = self
            .command_handle
            .lock()
            .expect("Claude command handle slot poisoned")
            .clone();
        match handle {
            Some(handle) => handle.begin_compaction(request).await,
            None => BackendCompactionStart::Deferred {
                reason: BackendCompactionDeferredReason::SessionInitializing,
            },
        }
    }

    async fn send(&self, input: AgentInput) -> bool {
        self.input_tx.send(input).is_ok()
    }

    async fn send_with_outcome(&self, input: AgentInput) -> crate::backend::SendOutcome {
        use crate::backend::SendOutcome;
        let handle = self
            .command_handle
            .lock()
            .expect("Claude command handle slot poisoned")
            .clone();
        let (payload, handle) = match (input, handle) {
            (AgentInput::SendMessage(payload), Some(handle)) => (payload, handle),
            (input, _) => {
                // Not a message (or the session is still starting): the pump
                // path is fine — such inputs can't collide with turn
                // admission.
                return if self.input_tx.send(input).is_ok() {
                    SendOutcome::Accepted
                } else {
                    SendOutcome::Closed
                };
            }
        };
        let retained = payload.clone();
        match handle.send_message_with_outcome(payload).await {
            Ok(ClaudeSendAdmission::Handled) => SendOutcome::Accepted,
            Ok(ClaudeSendAdmission::Busy) => SendOutcome::Busy(AgentInput::SendMessage(retained)),
            Err(err) => {
                tracing::error!("Failed to send Claude message: {err}");
                SendOutcome::Closed
            }
        }
    }

    async fn update_session_settings(
        &mut self,
        payload: protocol::SetSessionSettingsPayload,
    ) -> Result<(), String> {
        // Routed through the same direct handle as `send_with_outcome` so a
        // settings update can never be overtaken by a later message that
        // bypassed the input pump.
        let handle = self
            .command_handle
            .lock()
            .expect("Claude command handle slot poisoned")
            .clone();
        match handle {
            Some(handle) => {
                handle
                    .execute(SessionCommand::UpdateSettings {
                        settings: session_settings_to_json(&payload.values),
                        persist: false,
                    })
                    .await
            }
            None => self
                .send(AgentInput::UpdateSessionSettings(payload))
                .await
                .then_some(())
                .ok_or_else(|| "backend terminated before applying session settings".to_owned()),
        }
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

    async fn shutdown(mut self) {
        if let Some(cancel) = self.startup_cancel_tx.take() {
            let _ = cancel.send(());
        }
    }
}

/// Write a user message to the claude CLI stdin in stream-json format.
async fn signal_ready(ready_tx: &ClaudeReadyTx, result: Result<(), String>) {
    let mut ready_tx = ready_tx.lock().await;
    if let Some(tx) = ready_tx.take() {
        let _ = tx.send(result);
    }
}
