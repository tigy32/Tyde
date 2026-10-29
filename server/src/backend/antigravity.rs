use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::LazyLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::backend::subprocess::{AsyncCommandGroup, AsyncGroupChild};
use protocol::{
    AgentInput, BackendAccessMode, BackendKind, CapacityBucket, CapacityBucketId, CapacityCoverage,
    CapacityMeasure, CapacityReport, CapacityReset, CapacityScope, CapacitySource,
    CapacityUnavailableReason, CapacityWindow, ChatEvent, MessageTokenUsage, ModelInfo,
    ModelRequestId, ModelRequestTokenUsage, ModelTurnId, SelectOption, SessionId,
    SessionSettingField, SessionSettingFieldType, SessionSettingValue, SessionSettingsSchema,
    SessionSettingsValues, SpawnCostHint, ToolExecutionOutcome, ToolExecutionResult,
    ToolRequestType, ToolUseData, ValueProvenance,
};
use serde_json::{Map, Value, json, to_value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::ChildStdin;
use tokio::sync::{Mutex, mpsc, oneshot};
use uuid::Uuid;

use crate::backend::antigravity_stream::{
    AgyFrame, AgyResult, AgyStep, AgyUsage, AgyUsageBucket, AgyUsageReport, MCP_DISPATCH_TOOL,
    STATE_ACTIVE, STATE_CANCELLED, STATE_DONE, STATE_ERROR, STEP_AGENT_RESPONSE, STEP_CHECKPOINT,
    STEP_SUBAGENT, STEP_SYSTEM_MESSAGE, STEP_TOOL, STEP_UNKNOWN, STEP_USER_INPUT, TranscriptReader,
    TranscriptToolCall, answers_from_result, exit_code_from_result, mcp_inner_call, parse_frame,
    subagent_request_type, tool_request_type, usage_report_from_frame,
};
use crate::backend::skill_projection::{
    DescriptionPolicy, ProjectionPolicy, SkillRefusal, create_private_dir, create_private_root,
    discard_wrapper, inspect_skill, write_wrapper,
};
use crate::backend::turn_emitter::{AgentName, ResponseHandle, StreamEndPayload, TurnEmitter};
use crate::backend::{
    Backend, BackendCompactionAvailability, BackendCompactionCapability,
    BackendCompactionCapabilityEvidence, BackendCompactionCoordinator,
    BackendCompactionNotDispatchedReason, BackendCompactionRequest, BackendCompactionStart,
    BackendCompactionUnavailableReason, BackendDiscoveryError, BackendEvent, BackendSession,
    BackendSpawnConfig, BackendStartupError, EventStream, StartupMcpServer, StartupMcpTransport,
    backend_fork_unsupported_message, render_combined_spawn_instructions,
    resolve_settings as resolve_backend_settings,
};
use crate::sub_agent::SubAgentEmitter;

const ANTIGRAVITY_AGENT_NAME: &str = "antigravity";
const ANTIGRAVITY_STARTUP_TIMEOUT: Duration = Duration::from_secs(120);
/// `/usage` answers locally in well under a second; this only bounds a probe
/// that has stopped answering.
const ANTIGRAVITY_CAPACITY_TIMEOUT: Duration = Duration::from_secs(30);
/// Bound on a CLI-answered command (`/help`, `/usage`, …) run as its own
/// `--print` invocation. These make no model request.
const ANTIGRAVITY_COMMAND_TIMEOUT: Duration = Duration::from_secs(60);
/// CLI commands that change configuration `agy` shares across every process —
/// model, effort, permissions, hooks, settings. Tyde owns those per session,
/// so they are not offered; the remaining CLI-answered commands are read-only.
const ANTIGRAVITY_CONFIGURATION_COMMANDS: [&str; 5] =
    ["config", "effort", "hooks", "model", "permissions"];
/// How often the account's quota is re-read. Refreshing on every turn would
/// spawn a process per turn to watch a number that moves in hours.
const ANTIGRAVITY_CAPACITY_REFRESH_INTERVAL: Duration = Duration::from_secs(120);
/// How long a wait for the shared MCP config lock has to reach before it is
/// worth telling someone about.
const ANTIGRAVITY_MCP_LOCK_WARN_AFTER: Duration = Duration::from_secs(5);
/// How often the contended lock is retried while waiting.
const ANTIGRAVITY_MCP_LOCK_POLL: Duration = Duration::from_millis(250);
/// How long `agy` gets to finalize its conversation after `SIGTERM`. Measured
/// at well under a second; the margin is for a loaded machine.
const ANTIGRAVITY_GRACEFUL_EXIT_TIMEOUT: Duration = Duration::from_secs(15);
/// `agy` ends a turn on its own after this long (`--print-timeout`). It is set to
/// 720 hours (1 month) so long-running release commands, compilation, and subagent
/// tasks are not aborted by arbitrary provider-side turn deadlines. Unresponsive or
/// wedged provider processes are recovered via user manual interrupt or session shutdown.
const ANTIGRAVITY_PRINT_TIMEOUT: &str = "720h";
const ANTIGRAVITY_SKILLS_ROOT_PREFIX: &str = "tyde-antigravity-skills-";

const ANTIGRAVITY_SKILL_PROJECTION: ProjectionPolicy = ProjectionPolicy {
    refused_resource_names: &[],
    description: DescriptionPolicy::Required,
};

/// The one entry Tyde owns in `agy`'s shared MCP config. Shared with the
/// bridge so the tool-name mapping and the config agree on it.
const ANTIGRAVITY_BRIDGE_SERVER_NAME: &str = crate::mcp_bridge::MANAGED_SERVER_NAME;

static ANTIGRAVITY_MCP_CONFIG_MUTEX: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

struct WorkspaceRootsRequest {
    roots: Vec<String>,
    reply: oneshot::Sender<Result<(), super::WorkspaceRelocationError>>,
}

#[derive(Clone)]
pub struct AntigravityBackend {
    workspace_tx: mpsc::UnboundedSender<WorkspaceRootsRequest>,
    settings_tx:
        mpsc::UnboundedSender<(SessionSettingsValues, oneshot::Sender<Result<(), String>>)>,
    input_tx: mpsc::UnboundedSender<AgentInput>,
    interrupt_tx: mpsc::UnboundedSender<()>,
    session_id: SessionId,
    provider_version: Option<String>,
    inner: Arc<AntigravityInner>,
}

struct AntigravityInner {
    relocation_wake: tokio::sync::watch::Sender<()>,
    emitter: Arc<TurnEmitter>,
    state: AntigravityStateActor,
}

type AntigravityStateUpdate = Box<dyn FnOnce(&mut AntigravityState) + Send>;

struct AntigravityStateActor {
    commands: mpsc::UnboundedSender<AntigravityStateUpdate>,
}
impl AntigravityStateActor {
    fn new(mut state: AntigravityState) -> Self {
        let (commands, mut inbox) = mpsc::unbounded_channel::<AntigravityStateUpdate>();
        crate::backend::subprocess::spawn(async move {
            while let Some(update) = inbox.recv().await {
                update(&mut state);
            }
        });
        Self { commands }
    }
    async fn with<R: Send + 'static>(
        &self,
        update: impl FnOnce(&mut AntigravityState) -> R + Send + 'static,
    ) -> R {
        let (reply, result) = oneshot::channel();
        assert!(
            self.commands
                .send(Box::new(move |state| {
                    let _ = reply.send(update(state));
                }))
                .is_ok(),
            "Antigravity state owner stopped while borrowed"
        );
        result
            .await
            .expect("Antigravity state owner dropped its update")
    }
}

struct AntigravityState {
    #[cfg(feature = "test-support")]
    native_pid: Option<u32>,
    model: String,
    /// True between accepting a message and seeing that turn's `result`.
    turn_active: bool,
    closing: bool,
    /// Resolves once the supervisor has killed the `agy` process and released
    /// its MCP config entries.
    shutdown_complete: Option<oneshot::Receiver<()>>,
}

/// Everything a spawned `agy` process needs, kept so the supervisor can restart
/// it after an interrupt without re-deriving any of it.
#[derive(Clone)]
struct AgyLaunch {
    workspace_roots: Vec<String>,
    primary_root: String,
    extra_roots: Vec<String>,
    access_mode: BackendAccessMode,
    model: String,
    /// `(name, value)` naming this session's bridge descriptor, applied to
    /// every `agy` process this session starts — including the ones a restart
    /// after an interrupt creates.
    bridge_env: Option<(String, String)>,
}

struct AntigravitySkillProjection {
    root: tempfile::TempDir,
}

impl AntigravitySkillProjection {
    fn path(&self) -> &Path {
        self.root.path()
    }
}

#[derive(Default)]
struct AntigravitySkillSetup {
    projection: Option<AntigravitySkillProjection>,
    degraded_notice: Option<String>,
}

fn prepare_antigravity_skills(
    skills: &[crate::backend::customization::ResolvedSkill],
) -> Result<AntigravitySkillSetup, String> {
    if skills.is_empty() {
        return Ok(AntigravitySkillSetup::default());
    }

    let mut inspected = Vec::new();
    let mut refusals = Vec::new();
    let mut claimed = BTreeSet::new();
    for skill in skills {
        match inspect_skill(skill, &mut claimed, ANTIGRAVITY_SKILL_PROJECTION) {
            Ok(entry) => inspected.push(entry),
            Err(reason) => refusals.push(SkillRefusal {
                name: skill.name.clone(),
                reason,
            }),
        }
    }

    let mut projection = None;
    if !inspected.is_empty() {
        let root = create_private_root(None, ANTIGRAVITY_SKILLS_ROOT_PREFIX)?;
        let skills_dir = root.path().join(".agents").join("skills");
        create_private_dir(&skills_dir)?;
        for entry in inspected {
            if let Err(reason) = write_wrapper(&skills_dir, &entry) {
                discard_wrapper(&skills_dir, &entry.projected.name);
                refusals.push(SkillRefusal {
                    name: entry.source_name,
                    reason,
                });
            }
        }
        if std::fs::read_dir(&skills_dir)
            .map_err(|err| format!("Failed to inspect Antigravity skill projection: {err}"))?
            .next()
            .is_some()
        {
            projection = Some(AntigravitySkillProjection { root });
        }
    }

    for refusal in &refusals {
        tracing::warn!("Antigravity skill projection: {}", refusal.describe());
    }
    let degraded_notice = (!refusals.is_empty()).then(|| {
        format!(
            "Antigravity started without some selected skills:\n{}",
            refusals
                .iter()
                .map(|refusal| format!("- {}", refusal.describe()))
                .collect::<Vec<_>>()
                .join("\n")
        )
    });

    Ok(AntigravitySkillSetup {
        projection,
        degraded_notice,
    })
}

fn prepare_antigravity_tool_policy(
    categories: &[protocol::ToolCategory],
) -> Result<Option<tempfile::TempDir>, String> {
    if categories.is_empty() {
        return Ok(None);
    }
    let root = create_private_root(None, "tyde-antigravity-tool-policy-")?;
    let customization_dir = root.path().join(".agents");
    create_private_dir(&customization_dir)?;
    let mut handlers = Vec::new();
    for category in categories {
        let tool = match category {
            protocol::ToolCategory::AskUser => "ask_question",
            protocol::ToolCategory::AgentDelegation => {
                return Err("Antigravity cannot enforce delegation exclusion".into());
            }
        };
        let response = json!({
            "decision": "deny",
            "reason": "This tool is unavailable because the session excludes user questions.",
        })
        .to_string();
        #[cfg(not(windows))]
        let command = format!("printf '%s' {}", crate::remote::shell_quote_arg(&response));
        #[cfg(windows)]
        let command = format!("echo {response}");
        handlers.push(json!({
            "matcher": tool,
            "hooks": [{"type": "command", "command": command, "timeout": 10}],
        }));
    }
    crate::backend::skill_projection::write_private_file(
        &customization_dir.join("hooks.json"),
        &json!({"tyde-tool-category-policy": {"PreToolUse": handlers}}).to_string(),
    )?;
    tracing::info!(
        category_count = categories.len(),
        "Installed Antigravity native tool-category deny hooks"
    );
    Ok(Some(root))
}

impl AgyLaunch {
    fn native_roots(&self) -> Result<Vec<String>, String> {
        super::execution_workspace_roots(
            &std::iter::once(self.primary_root.clone())
                .chain(self.extra_roots.iter().cloned())
                .collect::<Vec<_>>(),
        )
    }

    fn args(&self, conversation_id: Option<&str>) -> Result<Vec<String>, String> {
        let mut args = vec![
            "--print-timeout".to_string(),
            ANTIGRAVITY_PRINT_TIMEOUT.to_string(),
            "--input-format".to_string(),
            "stream-json".to_string(),
            "--output-format".to_string(),
            "stream-json".to_string(),
        ];
        match self.access_mode {
            // `agy` has no workspace-write middle mode. ReadOnly is advisory, so it
            // must use the non-sandbox path to let build/test commands write target/.
            BackendAccessMode::Unrestricted | BackendAccessMode::ReadOnly => {
                args.push("--dangerously-skip-permissions".to_string())
            }
            BackendAccessMode::EnforcedReadOnly => {}
        }
        args.push("--model".to_string());
        args.push(self.model.clone());
        if let Some(conversation_id) = conversation_id {
            args.push(format!("--conversation={conversation_id}"));
        }
        args.push("--add-dir".to_string());
        args.push(
            tyde_process_env::launch::WorkspacePath::host(&self.primary_root)?
                .execution_string()?,
        );
        for root in &self.extra_roots {
            args.push("--add-dir".to_string());
            args.push(tyde_process_env::launch::WorkspacePath::host(root)?.execution_string()?);
        }
        Ok(args)
    }
}

/// A running `agy` process and the two ends of its stream-json conversation.
struct AgyProcess {
    child: AsyncGroupChild,
    stdin: ChildStdin,
    frames_rx: mpsc::UnboundedReceiver<Result<AgyFrame, String>>,
    conversation_id: String,
}

impl AgyProcess {
    /// Starts `agy` and blocks until its `init` frame names the conversation.
    ///
    /// A resumed process is handed the id it must adopt; a fresh one learns its
    /// own. Either way the id `init` reports is the one this returns, so a
    /// resume that silently landed on a different conversation is caught here
    /// rather than becoming a session that writes to the wrong transcript.
    async fn start(
        launch: &AgyLaunch,
        resume: Option<&str>,
        emitter: Arc<TurnEmitter>,
    ) -> Result<Self, String> {
        if launch.access_mode == BackendAccessMode::EnforcedReadOnly {
            return Err("Antigravity cannot enforce read-only workspace access".into());
        }
        let mut command = crate::backend::subprocess::command("agy")?;
        command.args(launch.args(resume)?);
        if let Some((name, value)) = launch.bridge_env.as_ref() {
            command.env(name, value);
        }
        command
            .current_dir(&launch.primary_root)
            .stdin(crate::backend::subprocess::ProcessStdio::Piped)
            .stdout(crate::backend::subprocess::ProcessStdio::Piped)
            .stderr(crate::backend::subprocess::ProcessStdio::Piped);

        let mut child = command
            .group_spawn()
            .await
            .map_err(|err| format!("Failed to start Antigravity CLI: {err:?}"))?;
        let stdin = child
            .take_stdin()
            .ok_or_else(|| "Failed to capture Antigravity stdin".to_string())?;
        let stdout = child
            .take_stdout()
            .ok_or_else(|| "Failed to capture Antigravity stdout".to_string())?;
        let stderr = child
            .take_stderr()
            .ok_or_else(|| "Failed to capture Antigravity stderr".to_string())?;

        let (frames_tx, mut frames_rx) = mpsc::unbounded_channel();
        crate::backend::subprocess::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        if frames_tx.send(parse_frame(trimmed)).is_err() {
                            return;
                        }
                    }
                    Ok(None) => return,
                    Err(err) => {
                        let _ = frames_tx
                            .send(Err(format!("Failed to read Antigravity stdout: {err}")));
                        return;
                    }
                }
            }
        });

        let stderr_emitter = Arc::clone(&emitter);
        crate::backend::subprocess::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if !line.trim().is_empty() {
                    stderr_emitter.subprocess_stderr(&line);
                }
            }
        });

        let init = tokio::time::timeout(ANTIGRAVITY_STARTUP_TIMEOUT, async {
            while let Some(frame) = frames_rx.recv().await {
                match frame? {
                    AgyFrame::Init(init) => return Ok(init.conversation_id),
                    // `agy` answers a startup failure — no credentials, a model
                    // the account cannot use — with a terminal `result` before
                    // any `init`. Its message is the only useful diagnostic.
                    AgyFrame::Result(result) => {
                        return Err(startup_failure_message(&result));
                    }
                    _ => continue,
                }
            }
            Err("Antigravity CLI exited before reporting a conversation".to_string())
        })
        .await
        .map_err(|_| {
            format!(
                "Antigravity CLI did not start within {}s",
                ANTIGRAVITY_STARTUP_TIMEOUT.as_secs()
            )
        })??;

        if let Some(expected) = resume
            && expected != init
        {
            return Err(format!(
                "Antigravity resumed conversation {init} when {expected} was requested"
            ));
        }

        Ok(Self {
            child,
            stdin,
            frames_rx,
            conversation_id: init,
        })
    }

    async fn send_turn(&mut self, message: &str) -> Result<(), String> {
        let line = serde_json::to_string(&json!({
            "event": "user",
            "message": { "content": message },
        }))
        .map_err(|err| format!("Failed to encode Antigravity turn: {err}"))?;
        self.stdin
            .write_all(format!("{line}\n").as_bytes())
            .await
            .map_err(|err| format!("Failed to send turn to Antigravity: {err}"))?;
        self.stdin
            .flush()
            .await
            .map_err(|err| format!("Failed to flush Antigravity stdin: {err}"))
    }

    /// Ends the turn the way an interrupt has to end it.
    ///
    /// `SIGKILL` stops the command that is running, but it also leaves the
    /// tool's step marked RUNNING in `agy`'s own conversation store, and
    /// resuming that conversation re-executes it. Measured 2026-08-25: a
    /// `sleep 20 && write proof` command killed with `SIGKILL` had not written
    /// its proof file at kill time and *had* written it 30 seconds after the
    /// resume — the cancelled command ran to completion anyway, which is
    /// exactly the defect `real_interruption` exists to catch. A new user turn
    /// sent immediately on resume does not pre-empt it either.
    ///
    /// `SIGTERM` gives `agy` the chance to finalize the step before it exits,
    /// and the same measurement then shows the proof file absent both at kill
    /// time and 30 seconds after the resume. Asking politely is platform
    /// specific — see `request_graceful_exit` — and where there is no way to
    /// ask, this falls back to the kill and its consequence.
    async fn terminate(&mut self) {
        if self.request_graceful_exit() {
            // Waiting on the provider to finish its own shutdown, not on local
            // state: if it will not exit, killing it still has to end it, and
            // the re-run on resume is the lesser of the two failures.
            let graceful =
                tokio::time::timeout(ANTIGRAVITY_GRACEFUL_EXIT_TIMEOUT, self.child.wait()).await;
            if graceful.is_ok() {
                return;
            }
            tracing::warn!(
                "Antigravity did not exit within {}s of the graceful stop; killing it, which \
                 leaves its in-flight tool step resumable",
                ANTIGRAVITY_GRACEFUL_EXIT_TIMEOUT.as_secs()
            );
        }
        // The job object takes the whole process tree with it, so the command
        // does stop either way. What is lost is `agy`'s chance to finalize the
        // step, which is what stops a resume from re-running it.
        let _ = self.child.kill().await;
        let _ = self.child.wait().await;
    }

    /// Asks `agy` to stop rather than killing it, where the platform has a way
    /// to ask.
    ///
    /// Returns whether the request was delivered, so the caller knows whether
    /// waiting for a clean exit is worth anything.
    #[cfg(unix)]
    fn request_graceful_exit(&mut self) -> bool {
        use command_group::{Signal, UnixChildExt};

        self.child.signal(Signal::SIGTERM).is_ok()
    }

    /// Windows has no `SIGTERM`, and the console-control event that comes
    /// closest needs the child spawned into its own process group plus a
    /// `kernel32` call — neither of which this crate has today, and neither of
    /// which can be verified from the machines this is developed on. So an
    /// interrupt here is the kill path: the command still stops, but `agy`
    /// never finalizes the step, and resuming that conversation re-runs it.
    #[cfg(windows)]
    fn request_graceful_exit(&mut self) -> bool {
        false
    }
}

fn startup_failure_message(result: &AgyResult) -> String {
    let detail = result
        .error
        .as_deref()
        .map(str::trim)
        .filter(|error| !error.is_empty())
        .unwrap_or_else(|| result.response.trim());
    if detail.is_empty() {
        "Antigravity CLI failed to start".to_string()
    } else {
        format!("Antigravity CLI failed to start: {detail}")
    }
}
/// The per-turn mapping state.
///
/// `agy` reports a turn as a flat, strictly ordered run of steps: an
/// `agent_response` step is one model request, and the `tool` steps it issued
/// follow it. The response that owns those tools is therefore already `DONE` by
/// the time the first of them arrives, so a response is held open until the
/// *next* model request starts or the turn ends. That is what lets a message
/// carry the tool calls it actually made instead of splitting each call onto a
/// message of its own.
struct TurnMapper {
    turn_id: ModelTurnId,
    model: String,
    response: Option<OpenResponse>,
    open_tools: BTreeMap<u64, OpenTool>,
    deferred_tools: BTreeMap<u64, AgyStep>,
    completed_steps: BTreeSet<u64>,
    turn_usage: AgyUsage,
    // None preserves a missing request measurement across deferred answers.
    message_generation_usage: Option<AgyUsage>,
    cumulative_usage: AgyUsage,
    /// Question cards this turn opened and deliberately did not close.
    pending_questions: Vec<String>,
    request_sequence: u32,
    transcript: TranscriptReader,
    /// The usage of a model request that issued tool calls. Its message does
    /// not exist yet: the `tool` steps that will populate it arrive after the
    /// `agent_response` step closes.
    pending_usage: Option<AgyUsage>,
}

struct OpenResponse {
    handle: ResponseHandle,
    step_index: u64,
    text: String,
    declarations: Vec<ToolUseData>,
    usage: Option<AgyUsage>,
}

struct OpenTool {
    tool_call_id: String,
    name: String,
    /// Kept so the completion can be reported in the same shape as the request.
    /// A `ModifyFile` that finishes as `Other` leaves the card with no `+A -B`
    /// footer, and a `RunCommand` with no exit code.
    tool_type: ToolRequestType,
}

impl TurnMapper {
    fn new(
        turn_id: String,
        model: String,
        transcript: TranscriptReader,
        cumulative: AgyUsage,
        message_generation_usage: Option<AgyUsage>,
    ) -> Self {
        Self {
            turn_id: ModelTurnId(turn_id),
            model,
            response: None,
            open_tools: BTreeMap::new(),
            deferred_tools: BTreeMap::new(),
            completed_steps: BTreeSet::new(),
            turn_usage: AgyUsage::default(),
            message_generation_usage,
            cumulative_usage: cumulative,
            pending_questions: Vec::new(),
            request_sequence: 0,
            transcript,
            pending_usage: None,
        }
    }

    async fn handle_step(&mut self, emitter: &TurnEmitter, step: AgyStep) {
        match step.step_type.as_str() {
            STEP_AGENT_RESPONSE => self.handle_agent_response(emitter, step),
            STEP_TOOL | STEP_SUBAGENT | STEP_UNKNOWN => {
                self.handle_tool_like(emitter, step).await;
            }
            // Tyde emits the user's own message, `agy` re-states the prompt it
            // was given, and a checkpoint is the CLI's private context
            // bookkeeping. A `system_message` is how a subagent's reply and a
            // background task's completion reach the model; the stream carries
            // no text for it, so there is nothing to show.
            STEP_USER_INPUT | STEP_CHECKPOINT | STEP_SYSTEM_MESSAGE => {}
            other => {
                tracing::debug!("Antigravity step type {other:?} has no Tyde mapping");
            }
        }
    }

    fn handle_agent_response(&mut self, emitter: &TurnEmitter, step: AgyStep) {
        // A new model request begins: whatever the previous one was holding
        // open, including the tools it declared, is now complete.
        if self
            .response
            .as_ref()
            .is_some_and(|open| open.step_index != step.step_index)
        {
            self.close_response(emitter);
        }

        if let Some(delta) = step.text_delta.as_deref().filter(|text| !text.is_empty()) {
            let open = self.ensure_response(emitter, step.step_index);
            emitter.stream_delta(&open.handle, delta);
            open.text.push_str(delta);
        }

        if step.state == STATE_DONE {
            if step.usage.is_none() && self.message_generation_usage.is_some() {
                tracing::warn!(
                    target: "tyde_antigravity_usage_diagnostic",
                    "Antigravity completed a request without usage; generation usage is unavailable"
                );
            }
            self.message_generation_usage = self
                .message_generation_usage
                .zip(step.usage)
                .map(|(total, usage)| total.saturating_add(usage));
        }
        if step.state == STATE_DONE
            && let Some(usage) = step.usage
        {
            self.turn_usage = self.turn_usage.saturating_add(usage);
            self.cumulative_usage = self.cumulative_usage.saturating_add(usage);
            self.request_sequence = self.request_sequence.saturating_add(1);
            emitter.model_request_token_usage(&ModelRequestTokenUsage {
                request_id: ModelRequestId {
                    turn_id: self.turn_id.clone(),
                    sequence: self.request_sequence,
                },
                request: usage.to_protocol(),
                turn: self.turn_usage.to_protocol(),
                cumulative: self.cumulative_usage.to_protocol(),
                model_context_window: None,
                current_context_usage: None,
                estimated_context_breakdown: None,
            });
            // Attach the request's own usage to the message it produced. The
            // response is not closed here: its tool steps have not arrived yet.
            if let Some(open) = self
                .response
                .as_mut()
                .filter(|open| open.step_index == step.step_index)
            {
                open.usage = Some(usage);
            } else {
                self.pending_usage = Some(usage);
            }
        }
    }

    fn ensure_response(&mut self, emitter: &TurnEmitter, step_index: u64) -> &mut OpenResponse {
        if self.response.is_none() {
            let handle = emitter.stream_start(Some(&self.model));
            self.response = Some(OpenResponse {
                handle,
                step_index,
                text: String::new(),
                declarations: Vec::new(),
                usage: self.pending_usage.take(),
            });
        }
        self.response.as_mut().expect("response just created")
    }

    async fn handle_tool_like(&mut self, emitter: &TurnEmitter, step: AgyStep) {
        let step_index = step.step_index;
        if self.completed_steps.contains(&step_index) {
            return;
        }

        let needs_call =
            step.step_type != STEP_SUBAGENT && !self.open_tools.contains_key(&step_index);
        let call = if needs_call {
            self.recover_tool_call(&step).await
        } else {
            None
        };
        if needs_call && call.is_none() && step.state == STATE_ACTIVE {
            // A request cannot be retyped once emitted. Wait for a later tool
            // update rather than permanently publishing the stream summary.
            self.deferred_tools.insert(step_index, step);
            return;
        }
        if self.deferred_tools.remove(&step_index).is_some() && call.is_some() {
            tracing::info!(step_index, "Antigravity recovered deferred tool arguments");
        }
        let tool_name = step
            .tool_name
            .clone()
            .or_else(|| step.tool_info.as_ref().and_then(|info| info.name.clone()))
            .or_else(|| call.as_ref().map(|call| call.name.clone()))
            .unwrap_or_else(|| step.step_type.clone());

        match step.state.as_str() {
            STATE_ACTIVE => {
                if !self.open_tools.contains_key(&step_index) {
                    self.open_tool(emitter, step_index, &tool_name, &step, call);
                }
            }
            STATE_DONE | STATE_ERROR => {
                // A tool whose ACTIVE never arrived still gets a card, so the
                // work shows up rather than vanishing.
                if !self.open_tools.contains_key(&step_index) {
                    self.open_tool(emitter, step_index, &tool_name, &step, call);
                }
                self.close_tool(emitter, step_index, &step);
                self.completed_steps.insert(step_index);
            }
            STATE_CANCELLED => {
                if !self.open_tools.contains_key(&step_index) {
                    self.open_tool(emitter, step_index, &tool_name, &step, call);
                }
                self.cancel_tool(emitter, step_index);
                self.completed_steps.insert(step_index);
            }
            other => {
                tracing::debug!("Antigravity tool step state {other:?} has no Tyde mapping");
            }
        }
    }

    async fn recover_tool_call(&mut self, step: &AgyStep) -> Option<TranscriptToolCall> {
        let tool_name = step.tool_name.as_deref().or_else(|| {
            step.tool_info
                .as_ref()
                .and_then(|info| info.name.as_deref())
        });
        let started = tokio::time::Instant::now();
        loop {
            self.transcript.refresh();
            let call = match tool_name {
                Some(name) => self.transcript.call_for_tool_step(step.step_index, name),
                None => self.transcript.unnamed_call_for_tool_step(step.step_index),
            }
            .cloned();
            if call.is_some() {
                return call;
            }
            if step.state == STATE_ACTIVE {
                if !self.deferred_tools.contains_key(&step.step_index) {
                    tracing::warn!(
                        step_index = step.step_index,
                        tool_name,
                        transcript = %self.transcript.path().display(),
                        "Antigravity tool arguments not yet available; deferring request"
                    );
                }
                return None;
            }
            if started.elapsed() >= Duration::from_secs(1) {
                tracing::warn!(
                    step_index = step.step_index,
                    tool_name,
                    transcript = %self.transcript.path().display(),
                    "Antigravity completed tool arguments unavailable; using stream summary"
                );
                return None;
            }
            // The CLI can publish a tool update before flushing its transcript.
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn cancel_tool(&mut self, emitter: &TurnEmitter, step_index: u64) {
        let Some(open) = self.open_tools.remove(&step_index) else {
            return;
        };
        // A question card outlives its turn so the user can answer it.
        if matches!(open.tool_type, ToolRequestType::AskUserQuestion { .. }) {
            self.pending_questions.push(open.tool_call_id);
            return;
        }
        emitter.tool_completed(
            &open.tool_call_id,
            ToolExecutionOutcome::Cancelled {
                message: "Tool execution was cancelled".to_string(),
            },
        );
    }

    fn open_tool(
        &mut self,
        emitter: &TurnEmitter,
        step_index: u64,
        tool_name: &str,
        step: &AgyStep,
        call: Option<TranscriptToolCall>,
    ) {
        let tool_call_id = format!("agy-{}-{step_index}", self.turn_id.0);
        let mut card_name = tool_name.to_string();

        // The stream's `tool_info.parameters` is a summary; the transcript has
        // the arguments the model really passed. Whichever is available is both
        // what the type is derived from and what the declaration carries, so
        // the card and the record can never disagree.
        let (provider_arguments, tool_type) = if step.step_type == STEP_SUBAGENT {
            let subagent = step
                .subagent_info
                .as_ref()
                .and_then(|info| info.subagents.first());
            let arguments = step
                .subagent_info
                .as_ref()
                .and_then(|info| to_value(info).ok())
                .unwrap_or(Value::Null);
            let tool_type =
                subagent
                    .map(subagent_request_type)
                    .unwrap_or_else(|| ToolRequestType::Other {
                        args: arguments.clone(),
                    });
            (arguments, tool_type)
        } else {
            let enriched = call.map(|call| call.args);
            let summary = step
                .tool_info
                .as_ref()
                .and_then(|info| info.parameters.clone())
                .unwrap_or(Value::Null);
            let (args, enriched) = match enriched {
                Some(args) => (args, true),
                None => (summary, false),
            };
            let tool_type = tool_request_type(tool_name, &args, enriched);
            // An MCP call's own arguments, not the dispatcher's envelope.
            if tool_name == MCP_DISPATCH_TOOL {
                let (inner_name, inner_args) = mcp_inner_call(&args);
                card_name = inner_name;
                (inner_args, tool_type)
            } else {
                (args, tool_type)
            }
        };

        // The provider's own arguments, never a re-serialization of the
        // normalized type. The normalized form already rides on the request;
        // copying it here would discard the only record of what the model
        // actually passed.
        // A native pre-tool denial is a failed attempt, not an answerable card.
        let tool_type = if step.state == STATE_ERROR
            && matches!(tool_type, ToolRequestType::AskUserQuestion { .. })
        {
            tracing::info!("Antigravity question attempt rejected before requesting user input");
            ToolRequestType::Other {
                args: provider_arguments.clone(),
            }
        } else {
            tool_type
        };
        let arguments = provider_arguments;
        let declaration = ToolUseData {
            tool_call_id: tool_call_id.clone(),
            name: card_name.clone(),
            arguments,
            content_offset: None,
        };

        let handle = {
            let open = self.ensure_response(emitter, step_index);
            open.declarations.push(declaration.clone());
            open.handle.clone()
        };
        emitter.declare_streaming_tools(&handle, vec![declaration]);
        emitter.tool_request(&tool_call_id, tool_type.clone());
        self.open_tools.insert(
            step_index,
            OpenTool {
                tool_call_id,
                name: card_name,
                tool_type,
            },
        );
    }

    fn close_tool(&mut self, emitter: &TurnEmitter, step_index: u64, step: &AgyStep) {
        let Some(open) = self.open_tools.remove(&step_index) else {
            return;
        };
        if step.state == STATE_ERROR {
            let message = step
                .tool_info
                .as_ref()
                .and_then(|info| info.error.as_ref())
                .and_then(|error| error.message.clone())
                .unwrap_or_else(|| format!("{} failed", open.name));
            emitter.tool_completed(
                &open.tool_call_id,
                ToolExecutionOutcome::Failed {
                    message,
                    details: None,
                    normalization_failure: None,
                    result: None,
                },
            );
            return;
        }

        // A question outlives its turn on purpose. `agy` answers its own
        // questions headlessly — "A1: User Skipped" — and ends the turn, but
        // that is precisely the shape Tyde expects: the turn ends so the user
        // can act on the card, and `TurnEmitter` exempts an open question from
        // the cancellation it applies to every other tool still running at
        // idle. Completing it here would terminalize the card behind the user
        // and make their answer arrive for an id that has already been retired.
        if matches!(open.tool_type, ToolRequestType::AskUserQuestion { .. }) {
            self.pending_questions.push(open.tool_call_id);
            return;
        }

        self.transcript.refresh();
        let transcript_result = self
            .transcript
            .result_for_tool_step(step_index)
            .map(str::to_string);
        let output = step
            .tool_info
            .as_ref()
            .and_then(|info| info.output.clone())
            .unwrap_or_default();
        let is_mcp_dispatch = step.tool_name.as_deref() == Some(MCP_DISPATCH_TOOL);
        emitter.tool_completed(
            &open.tool_call_id,
            ToolExecutionOutcome::Succeeded {
                result: tool_execution_result(
                    &open.tool_type,
                    &output,
                    transcript_result.as_deref(),
                    is_mcp_dispatch,
                ),
            },
        );
    }

    fn close_response(&mut self, emitter: &TurnEmitter) {
        let Some(open) = self.response.take() else {
            return;
        };
        if open.usage.is_none() && self.message_generation_usage.is_some() {
            tracing::warn!(
                target: "tyde_antigravity_usage_diagnostic",
                "Antigravity closed a response without usage; generation usage is unavailable"
            );
            self.message_generation_usage = None;
        }
        // A response with no text and no tool calls would render as an empty
        // bubble. `agy` produces one whenever a model request is pure
        // bookkeeping, so it is dropped rather than published.
        if open.text.is_empty() && open.declarations.is_empty() {
            emitter.stream_end(
                open.handle,
                StreamEndPayload {
                    content: String::new(),
                    ..Default::default()
                },
            );
            return;
        }
        emitter.stream_end(
            open.handle,
            StreamEndPayload {
                content: open.text,
                model_info: Some(ModelInfo {
                    model: self.model.clone(),
                }),
                token_usage: open.usage.map(|usage| match self.message_generation_usage {
                    Some(total) => MessageTokenUsage::request_and_turn_known(
                        usage.to_protocol(),
                        total.to_protocol(),
                    ),
                    None => MessageTokenUsage::request_known(usage.to_protocol()),
                }),
                tool_calls: open.declarations,
                ..Default::default()
            },
        );
    }

    fn take_pending_questions(&mut self) -> Vec<String> {
        std::mem::take(&mut self.pending_questions)
    }

    fn close_open_tools_as_cancelled(&mut self, emitter: &TurnEmitter, message: &str) {
        for (step_index, open) in std::mem::take(&mut self.open_tools) {
            self.completed_steps.insert(step_index);
            // Question cards outlive their turn so the user can act on them.
            if matches!(open.tool_type, ToolRequestType::AskUserQuestion { .. }) {
                self.pending_questions.push(open.tool_call_id);
            } else {
                emitter.tool_completed(
                    &open.tool_call_id,
                    ToolExecutionOutcome::Cancelled {
                        message: message.to_string(),
                    },
                );
            }
        }
    }

    /// Drops the mapper's own bookkeeping for an interrupted turn.
    ///
    /// The cards themselves are closed by `TurnEmitter::operation_cancelled`,
    /// which discards the open response and completes every pending tool as
    /// cancelled. Doing it here as well would report each card twice.
    fn abandon_open_work(&mut self) {
        self.open_tools.clear();
        self.deferred_tools.clear();
        self.completed_steps.clear();
        self.response = None;
    }
}
/// Owns the `agy` process for the life of the session.
///
/// One process serves every turn — measured, a second turn on the same process
/// recalled what the first was told — so the supervisor's job is to keep it
/// alive, feed it one NDJSON line per turn, and rebuild it around an interrupt.
struct Supervisor {
    inner: Arc<AntigravityInner>,
    launch: AgyLaunch,
    brain_dir: PathBuf,
    conversation_db: PathBuf,
    /// Deleted when the supervisor exits, taking this session's credentials
    /// with it.
    descriptor_dir: Option<tempfile::TempDir>,
    /// Keeps the native `.agents/skills` tree alive across every turn and
    /// process restart in this session.
    skill_projection: Option<AntigravitySkillProjection>,
    tool_policy_projection: Option<tempfile::TempDir>,
    mapper: Option<TurnMapper>,
    cumulative: AgyUsage,
    // Deferred answers keep working across native turns, without an idle edge.
    pending_generation_usage: Option<AgyUsage>,
    turn_counter: u64,
    /// Question cards waiting on a human. They survive the turn that asked, and
    /// are cleared either by an answer or by a cancel.
    pending_questions: Vec<String>,
    /// Answers the user gave while a turn was still running, in the order they
    /// were given. `agy` runs one turn at a time and holds a mid-turn stdin
    /// message until the running turn's `result`, so each answer becomes the
    /// next turn only once the current one has finished.
    deferred_answers: VecDeque<String>,
    /// When the account's quota was last read, so a busy session does not spawn
    /// a probe per turn.
    capacity_read_at: Option<std::time::Instant>,
    shutdown_complete: oneshot::Sender<()>,
    subagent_emitter: Option<Arc<dyn SubAgentEmitter>>,
    native_pending: BTreeMap<u64, (String, Vec<super::antigravity_stream::AgySubagent>)>,
    native_sessions: BTreeSet<String>,
    native_children: tokio::task::JoinSet<Result<(), String>>,
    native_child_failure: Option<String>,
    session_id: String,
    /// The CLI-answered commands advertised for this session. An invoking
    /// message runs as its own `--print` invocation, never as a turn.
    slash_commands: Option<protocol::SlashCommandCatalog>,
    /// A model change arrived while a turn was running. The process serving
    /// that turn keeps it; the next turn starts on a relaunched process.
    relaunch_before_next_turn: bool,
}

struct InitialTurn {
    provider_message: String,
    user_message: String,
    origin: Option<protocol::MessageOrigin>,
}

impl Supervisor {
    async fn fail_native_child(&mut self, error: String) {
        if self.native_child_failure.is_some() {
            return;
        }
        self.inner.state.with(|state| state.closing = true).await;
        self.native_child_failure = Some(error.clone());
        self.inner.emitter.backend_error(&error);
    }

    async fn run(
        mut self,
        mut process: AgyProcess,
        mut workspace_rx: mpsc::UnboundedReceiver<WorkspaceRootsRequest>,
        mut settings_rx: mpsc::UnboundedReceiver<(
            SessionSettingsValues,
            oneshot::Sender<Result<(), String>>,
        )>,
        mut input_rx: mpsc::UnboundedReceiver<AgentInput>,
        mut interrupt_rx: mpsc::UnboundedReceiver<()>,
        initial_turn: Option<InitialTurn>,
    ) {
        let emitter = Arc::clone(&self.inner.emitter);

        if let Some(turn) = initial_turn
            && !self
                .start_turn(
                    &mut process,
                    &turn.provider_message,
                    Some((&turn.user_message, turn.origin)),
                )
                .await
        {
            self.shutdown(process).await;
            return;
        }

        loop {
            tokio::select! {
                biased;

                interrupt = interrupt_rx.recv() => {
                    let Some(()) = interrupt else { break };
                    if !self.interrupt(&mut process).await {
                        break;
                    }
                    if !self.start_deferred_answer(&mut process).await {
                        break;
                    }
                }

                Some(result) = self.native_children.join_next(), if !self.native_children.is_empty() => {
                    let result = result.unwrap_or_else(|error| Err(format!("Native child relay task failed: {error}")));
                    if let Err(error) = result {
                        self.fail_native_child(error).await;
                        break;
                    }
                    self.inner.relocation_wake.send_replace(());
                }

                frame = process.frames_rx.recv() => {
                    match frame {
                        Some(Ok(frame)) => {
                            self.handle_frame(frame).await;
                            if self.native_child_failure.is_some() { break; }
                            if !self.start_deferred_answer(&mut process).await {
                                break;
                            }
                        }
                        // A line we cannot read is the provider telling us
                        // something we do not understand. Surfacing it beats
                        // dropping it and reporting a turn that quietly lost
                        // half its events.
                        Some(Err(err)) => emitter.backend_error(&err),
                        None => {
                            if !self.deferred_answers.is_empty() {
                                emitter.backend_error(&format!(
                                    "Antigravity CLI exited before {} answered question(s) \
                                     could be delivered",
                                    self.deferred_answers.len()
                                ));
                                self.deferred_answers.clear();
                            }
                            if self.inner.state.with(|state| state.turn_active).await {
                                emitter.backend_error(
                                    "Antigravity CLI exited while a turn was still running",
                                );
                                self.finish_turn().await;
                            }
                            break;
                        }
                    }
                }

                () = tokio::time::sleep(Duration::from_millis(50)),
                    if self.mapper.as_ref().is_some_and(|mapper| !mapper.deferred_tools.is_empty()) =>
                {
                    if let Some(mapper) = self.mapper.as_mut() {
                        let pending = mapper.deferred_tools.values().cloned().collect::<Vec<_>>();
                        for step in pending {
                            mapper.handle_tool_like(&emitter, step).await;
                        }
                    }
                }



                update = workspace_rx.recv() => {
                    let Some(WorkspaceRootsRequest { roots, reply }) = update else { break };
                    let result = if input_rx.is_empty() {
                        self.set_workspace_roots(&mut process, roots).await
                    } else {
                        Err(super::WorkspaceRelocationError::Busy)
                    };
                    let _ = reply.send(result);
                }

                update = settings_rx.recv() => {
                    let Some((values, reply)) = update else { break };
                    let _ = reply.send(self.apply_settings(&mut process, values).await);
                }

                incoming = input_rx.recv() => {
                    let Some(input) = incoming else { break };
                    if !self.handle_input(&mut process, input).await {
                        break;
                    }
                }
            }
        }

        self.shutdown(process).await;
    }

    /// `agy` has no cancel message: its stdin vocabulary is user turns only,
    /// and SIGINT kills the process outright while reporting a bogus "timeout
    /// waiting for response". So an interrupt is a kill plus a resume, which is
    /// safe because `--conversation=<id>` restores the conversation exactly.
    async fn interrupt(&mut self, process: &mut AgyProcess) -> bool {
        if let Some(mapper) = self.mapper.as_mut() {
            mapper.abandon_open_work();
        }
        self.mapper = None;
        self.pending_generation_usage = Some(AgyUsage::default());
        // `operation_cancelled` completes every pending tool as cancelled,
        // including any question still waiting on the user.
        self.pending_questions.clear();
        self.inner
            .state
            .with(|state| state.turn_active = false)
            .await;
        self.inner
            .emitter
            .operation_cancelled("Antigravity turn cancelled.");

        let conversation_id = process.conversation_id.clone();
        process.terminate().await;
        match AgyProcess::start(
            &self.launch,
            Some(&conversation_id),
            Arc::clone(&self.inner.emitter),
        )
        .await
        {
            Ok(restarted) => {
                *process = restarted;
                #[cfg(feature = "test-support")]
                {
                    let pid = process.child.id();
                    self.inner
                        .state
                        .with(move |state| state.native_pid = pid)
                        .await;
                }
                self.relaunch_before_next_turn = false;
                true
            }
            Err(err) => {
                self.inner.emitter.backend_error(&format!(
                    "Antigravity could not resume after the interrupt: {err}"
                ));
                false
            }
        }
    }

    /// Answers an advertised CLI command as its own `--print` run and shows
    /// the CLI's reply as the response. The persistent process is not
    /// involved: no turn starts there and no model request is made.
    async fn run_cli_command(&mut self, message: &str) -> bool {
        let emitter = Arc::clone(&self.inner.emitter);
        emitter.user_message(message, None, None);
        emitter.typing_status_changed(true);
        let model = self.launch.model.clone();
        let cwd = PathBuf::from(&self.launch.primary_root);
        match run_antigravity_cli_command("agy", Some(&model), Some(&cwd), message.trim()).await {
            Ok(output) if output.error.is_none() => {
                let text = output.response.trim_end().to_owned();
                let response = emitter.stream_start(Some(&model));
                emitter.stream_delta(&response, &text);
                emitter.stream_end(
                    response,
                    StreamEndPayload {
                        content: text,
                        model_info: Some(ModelInfo { model }),
                        ..Default::default()
                    },
                );
            }
            Ok(output) => emitter.error_message(&output.error.unwrap_or_default()),
            Err(error) => emitter.error_message(&error),
        }
        emitter.typing_status_changed(false);
        true
    }

    async fn handle_input(&mut self, process: &mut AgyProcess, input: AgentInput) -> bool {
        match input {
            AgentInput::SendMessage(payload) => {
                let superseded = if payload.tool_response.is_none()
                    && matches!(payload.origin, None | Some(protocol::MessageOrigin::User))
                {
                    self.pending_questions
                        .iter()
                        .chain(
                            self.mapper
                                .iter()
                                .flat_map(|mapper| mapper.pending_questions.iter()),
                        )
                        .cloned()
                        .collect::<Vec<_>>()
                } else {
                    Vec::new()
                };
                if let Some(response) = payload.tool_response.clone() {
                    if !self.answer_question(response).await {
                        return true;
                    }
                    if self.mapper.is_some() {
                        self.deferred_answers.push_back(payload.message);
                        return true;
                    }
                }
                if payload.tool_response.is_none()
                    && let Some(catalog) = self.slash_commands.as_ref()
                    && catalog.invoked_by(&payload.message).is_some()
                {
                    let accepted = self.run_cli_command(&payload.message).await;
                    if accepted {
                        self.withdraw_questions(&superseded);
                    }
                    return accepted;
                }
                // A tool response is not a chat message, so it produces no user
                // bubble — the card the user acted on is the record of it.
                let echo = payload.tool_response.is_none()
                    && payload.origin != Some(protocol::MessageOrigin::HostRestart);
                let echoed = echo.then_some((payload.message.as_str(), payload.origin));
                let accepted = self.start_turn(process, &payload.message, echoed).await;
                if accepted {
                    self.withdraw_questions(&superseded);
                }
                accepted
            }
            AgentInput::UpdateSessionSettings(payload) => {
                match self.apply_settings(process, payload.values).await {
                    Ok(()) => true,
                    Err(err) => {
                        self.inner.emitter.backend_error(&err);
                        false
                    }
                }
            }
            AgentInput::GoalControl(_)
            | AgentInput::EditQueuedMessage(_)
            | AgentInput::CancelQueuedMessage(_)
            | AgentInput::SendQueuedMessageNow(_)
            | AgentInput::SteerMessage(_) => {
                panic!(
                    "queued-message inputs must be handled by the agent actor before reaching the \
                     backend"
                );
            }
        }
    }

    fn withdraw_questions(&mut self, superseded: &[String]) {
        if !superseded.is_empty() {
            tracing::info!(
                question_count = superseded.len(),
                "Antigravity withdrawing questions superseded by user input"
            );
        }
        self.pending_questions.retain(|id| !superseded.contains(id));
        if let Some(mapper) = self.mapper.as_mut() {
            mapper
                .pending_questions
                .retain(|id| !superseded.contains(id));
        }
        for tool_call_id in superseded {
            self.inner
                .emitter
                .cancel_pending_tool(tool_call_id, "Question superseded by a new user message");
        }
    }

    /// A rejected response starts no provider work. A running turn keeps its
    /// own lifecycle; otherwise the rejection is bracketed as its own turn so
    /// the accepted response settles.
    fn reject_tool_response(&self, message: &str) {
        let bracketed = self.mapper.is_none();
        if bracketed {
            self.inner.emitter.typing_status_changed(true);
        }
        self.inner.emitter.backend_error(message);
        if bracketed {
            self.inner.emitter.typing_status_changed(false);
        }
    }

    /// Closes the card the user acted on, so their answer and the transcript
    /// agree, and reports a mismatch rather than silently dropping it.
    async fn answer_question(&mut self, response: protocol::SendMessageToolResponse) -> bool {
        let protocol::SendMessageToolResponse::AskUserQuestion {
            tool_call_id,
            answer,
            ..
        } = response
        else {
            self.reject_tool_response(
                "Antigravity received a plan-approval response, which it never requests",
            );
            return false;
        };
        // A question asked earlier in the running turn is still held by that
        // turn's mapper; it moves here only when the turn finishes.
        let held = [
            Some(&mut self.pending_questions),
            self.mapper
                .as_mut()
                .map(|mapper| &mut mapper.pending_questions),
        ]
        .into_iter()
        .flatten()
        .find_map(|pending| {
            let position = pending.iter().position(|id| *id == tool_call_id)?;
            pending.remove(position);
            Some(())
        });
        if held.is_none() {
            self.reject_tool_response(
                "Antigravity received an answer for a question that is not waiting on one",
            );
            return false;
        }
        self.inner.emitter.tool_completed(
            &tool_call_id,
            ToolExecutionOutcome::Succeeded {
                result: ToolExecutionResult::Other {
                    result: json!({ "answer": answer }),
                },
            },
        );
        true
    }

    async fn start_turn(
        &mut self,
        process: &mut AgyProcess,
        message: &str,
        echoed_user_message: Option<(&str, Option<protocol::MessageOrigin>)>,
    ) -> bool {
        if self.relaunch_before_next_turn
            && let Err(err) = self.relaunch(process).await
        {
            self.inner.emitter.backend_error(&err);
            return false;
        }
        let message = match super::workspace_prompt(message, Some(&self.launch.workspace_roots)) {
            Ok(message) => message,
            Err(error) => {
                self.inner.emitter.backend_error(&error);
                self.inner.emitter.typing_status_changed(false);
                return false;
            }
        };
        self.turn_counter += 1;
        let model = self
            .inner
            .state
            .with(|state| {
                state.turn_active = true;
                state.model.clone()
            })
            .await;
        self.mapper = Some(TurnMapper::new(
            format!("{}-{}", process.conversation_id, self.turn_counter),
            model,
            TranscriptReader::new(&self.brain_dir, &process.conversation_id),
            self.cumulative,
            self.pending_generation_usage.replace(AgyUsage::default()),
        ));
        if let Some((user_message, origin)) = echoed_user_message {
            self.inner.emitter.user_message(user_message, None, origin);
        }
        self.inner.emitter.typing_status_changed(true);
        if let Err(err) = process.send_turn(&message).await {
            self.inner.emitter.backend_error(&err);
            self.inner
                .state
                .with(|state| state.turn_active = false)
                .await;
            self.inner.emitter.typing_status_changed(false);
            self.mapper = None;
            return false;
        }
        true
    }

    async fn handle_frame(&mut self, frame: AgyFrame) {
        match frame {
            AgyFrame::Step(step) => {
                let native_spawn = step.step_type == STEP_SUBAGENT
                    || step.tool_name.as_deref() == Some("invoke_subagent");
                if let Some(mapper) = self.mapper.as_mut() {
                    if native_spawn && !matches!(step.state.as_str(), STATE_ERROR | STATE_CANCELLED)
                    {
                        let pending =
                            self.native_pending
                                .entry(step.step_index)
                                .or_insert_with(|| {
                                    (
                                        format!("agy-{}-{}", mapper.turn_id.0, step.step_index),
                                        Vec::new(),
                                    )
                                });
                        if step.state == STATE_DONE
                            && let Some(info) = &step.subagent_info
                        {
                            pending.1 = info.subagents.clone();
                        }
                        tracing::info!(
                            step = step.step_index,
                            state = step.state,
                            children = pending.1.len(),
                            "Antigravity authoritative native child metadata"
                        );
                    }
                    mapper.handle_step(&self.inner.emitter, step).await;
                }
                self.discover_native_children().await;
            }
            AgyFrame::Result(result) => {
                if let Some(mapper) = self.mapper.as_ref() {
                    self.cumulative = mapper.cumulative_usage;
                }
                if result.status.eq_ignore_ascii_case("ERROR") {
                    let detail = result
                        .error
                        .as_deref()
                        .map(str::trim)
                        .filter(|error| !error.is_empty())
                        .unwrap_or("Antigravity reported a failed turn");
                    self.inner.emitter.error_message(detail);
                }
                self.finish_turn().await;
            }
            // `command_result` is emitted only by the read-only slash commands,
            // which never run inside a turn, and `init` was consumed at start.
            AgyFrame::CommandResult(_) | AgyFrame::Init(_) => {}
            AgyFrame::Unrecognized { event } => {
                tracing::debug!("Antigravity emitted unrecognized event {event:?}");
            }
        }
    }

    async fn discover_native_children(&mut self) {
        let Some(subagents) = self.subagent_emitter.clone() else {
            if !self.native_pending.is_empty() {
                let error = "Antigravity native children have no owning event emitter".to_owned();
                self.fail_native_child(error).await;
            }
            return;
        };
        for (step, (tool_id, children)) in self.native_pending.clone() {
            if children.is_empty() {
                continue;
            }
            if children.iter().any(|child| child.conversation_id.is_none()) {
                continue;
            }
            self.native_pending.remove(&step);
            for child_metadata in children {
                let id = child_metadata
                    .conversation_id
                    .expect("validated native metadata");
                if !self.native_sessions.insert(id.clone()) {
                    continue;
                }
                let name = child_metadata
                    .role
                    .or(child_metadata.type_name)
                    .unwrap_or_else(|| "Antigravity subagent".to_owned());
                let description = child_metadata.initial_prompt.unwrap_or_default();
                match subagents
                    .on_subagent_spawned(
                        tool_id.clone(),
                        name.clone(),
                        description.clone(),
                        ANTIGRAVITY_AGENT_NAME.to_owned(),
                        Some(SessionId(id.clone())),
                    )
                    .await
                {
                    Ok(child) => {
                        tracing::info!("Registered authoritative Antigravity native child");
                        self.inner
                            .emitter
                            .tool_progress(&protocol::ToolProgressData {
                                tool_call_id: tool_id.clone(),
                                execution_mode: protocol::ToolExecutionMode::Background,
                                cancellable: false,
                                update: protocol::ToolProgressUpdate::SubAgent(
                                    protocol::SubAgentProgress {
                                        backend_kind: None,
                                        agent_id: child.agent_id.clone(),
                                        agent_name: name.clone(),
                                        last_tool_name: None,
                                        tool_calls: 0,
                                        completed: false,
                                        status: protocol::SubAgentProgressStatus::Running,
                                    },
                                ),
                            });
                        let child_db = self.conversation_db.with_file_name(format!("{id}.db"));
                        let emitter = Arc::clone(&self.inner.emitter);
                        let tool_id = tool_id.clone();
                        let name = name.clone();
                        let relay = AgyNativeChildRelay {
                            child_db,
                            tool_call_id: tool_id,
                            name,
                            child,
                            emitter,
                            session_id: SessionId(id),
                            description,
                            subagents: subagents.clone(),
                        };
                        self.native_children
                            .spawn(crate::backend::subprocess::inherit_execution(relay.run()));
                    }
                    Err(error) => {
                        let error = format!("Antigravity child registration failed: {error}");
                        self.fail_native_child(error).await;
                        return;
                    }
                }
            }
        }
    }

    async fn finish_turn(&mut self) {
        self.discover_native_children().await;
        if !self.native_pending.is_empty() {
            let error =
                "Antigravity omitted native child session metadata at turn completion".to_owned();
            self.fail_native_child(error).await;
        }
        if let Some(mut mapper) = self.mapper.take() {
            for (_, mut step) in std::mem::take(&mut mapper.deferred_tools) {
                step.state = STATE_CANCELLED.to_string();
                mapper.handle_tool_like(&self.inner.emitter, step).await;
            }
            mapper.close_response(&self.inner.emitter);
            mapper.close_open_tools_as_cancelled(
                &self.inner.emitter,
                "Antigravity turn ended before tool execution completed",
            );
            self.pending_questions
                .extend(mapper.take_pending_questions());
            if !self.deferred_answers.is_empty() {
                tracing::debug!("Antigravity retained message accounting across a deferred answer");
                self.pending_generation_usage = mapper.message_generation_usage;
            }
        }
        // A deferred answer starts its turn as soon as this one is over, so
        // the session never reports an idle it is about to leave.
        if self.deferred_answers.is_empty() {
            self.inner
                .state
                .with(|state| state.turn_active = false)
                .await;
            self.inner.emitter.typing_status_changed(false);
        }
        if self.native_child_failure.is_none() {
            self.refresh_capacity().await;
        }
    }

    /// Starts the oldest answer that arrived mid-turn once no turn is running.
    async fn start_deferred_answer(&mut self, process: &mut AgyProcess) -> bool {
        if self.mapper.is_some() {
            return true;
        }
        let Some(answer) = self.deferred_answers.pop_front() else {
            return true;
        };
        self.start_turn(process, &answer, None).await
    }

    /// Re-reads the account's remaining quota, at most once per interval.
    ///
    /// Turn boundaries are the trigger because that is when the number has
    /// just moved and when the user is most likely to look at it.
    async fn refresh_capacity(&mut self) {
        if self
            .capacity_read_at
            .is_some_and(|last| last.elapsed() < ANTIGRAVITY_CAPACITY_REFRESH_INTERVAL)
        {
            return;
        }
        let emitter = self.subagent_emitter.clone();
        let Some(emitter) = emitter else {
            return;
        };
        self.capacity_read_at = Some(std::time::Instant::now());
        crate::backend::subprocess::spawn(async move {
            let state = match read_antigravity_capacity().await {
                Ok(report) => protocol::BackendCapacityState::Known { report },
                Err(reason) => protocol::BackendCapacityState::Unavailable { reason },
            };
            emitter.on_backend_capacity(BackendKind::Antigravity, state);
        });
    }

    async fn set_workspace_roots(
        &mut self,
        process: &mut AgyProcess,
        roots: Vec<String>,
    ) -> Result<(), super::WorkspaceRelocationError> {
        if let Some(error) = &self.native_child_failure {
            return Err(super::WorkspaceRelocationError::Rejected(error.clone()));
        }
        let (closing, turn_active) = self
            .inner
            .state
            .with(|state| (state.closing, state.turn_active))
            .await;
        if closing
            || turn_active
            || self.mapper.is_some()
            || !self.pending_questions.is_empty()
            || !self.native_pending.is_empty()
            || !self.native_children.is_empty()
        {
            return Err(super::WorkspaceRelocationError::Busy);
        }
        let mut launch = self.launch.clone();
        launch.primary_root = roots[0].clone();
        launch.workspace_roots = roots.clone();
        launch.extra_roots.clear();
        if let Some(projection) = self.skill_projection.as_ref() {
            launch.extra_roots.push(
                projection
                    .path()
                    .to_str()
                    .ok_or_else(|| {
                        super::WorkspaceRelocationError::Rejected(
                            "Antigravity skill projection path is not UTF-8".to_owned(),
                        )
                    })?
                    .to_owned(),
            );
        }
        if let Some(projection) = self.tool_policy_projection.as_ref() {
            launch.extra_roots.push(
                projection
                    .path()
                    .to_str()
                    .ok_or_else(|| {
                        super::WorkspaceRelocationError::Rejected(
                            "Antigravity tool policy path is not UTF-8".to_owned(),
                        )
                    })?
                    .to_owned(),
            );
        }
        tracing::info!(session_id = %self.session_id, ?roots, "Changing Antigravity workspace roots");
        // The resumed native trajectory, not CLI --add-dir, owns tool workspaces.
        process.terminate().await;
        let replacement_roots = launch
            .native_roots()
            .map_err(super::WorkspaceRelocationError::Rejected)?;
        let metadata = relocate_antigravity_metadata(
            &self.conversation_db,
            &tyde_process_env::launch::WorkspacePath::host(&self.launch.primary_root)
                .and_then(|path| path.execution_string())
                .map_err(super::WorkspaceRelocationError::Rejected)?,
            &replacement_roots,
        );
        let result = match &metadata {
            Ok(_) => {
                AgyProcess::start(
                    &launch,
                    Some(&self.session_id),
                    Arc::clone(&self.inner.emitter),
                )
                .await
            }
            Err(error) => Err(error.clone()),
        };
        match result {
            Ok(replacement) => *process = replacement,
            Err(error) => {
                if let Ok((before, after)) = metadata {
                    replace_antigravity_metadata(&self.conversation_db, &after, &before)
                        .map_err(super::WorkspaceRelocationError::OutcomeUnknown)?;
                }
                *process = AgyProcess::start(&self.launch, Some(&self.session_id), Arc::clone(&self.inner.emitter)).await
                    .map_err(|recovery| super::WorkspaceRelocationError::OutcomeUnknown(format!("Workspace relocation failed: {error}; original runtime recovery failed: {recovery}")))?;
                #[cfg(feature = "test-support")]
                {
                    let pid = process.child.id();
                    self.inner
                        .state
                        .with(move |state| state.native_pid = pid)
                        .await;
                }
                return Err(super::WorkspaceRelocationError::OutcomeUnknown(error));
            }
        }
        #[cfg(feature = "test-support")]
        {
            let pid = process.child.id();
            self.inner
                .state
                .with(move |state| state.native_pid = pid)
                .await;
        }
        self.launch = launch;
        self.relaunch_before_next_turn = false;
        tracing::info!(session_id = %self.session_id, ?roots, "Antigravity resumed in the new workspace roots");
        Ok(())
    }

    /// A model change is a process restart: `--model` is a launch flag and
    /// `agy` takes no equivalent over stdin. The conversation survives it.
    async fn apply_settings(
        &mut self,
        process: &mut AgyProcess,
        values: SessionSettingsValues,
    ) -> Result<(), String> {
        let model = selected_model(&values, Some(Path::new(&self.launch.primary_root))).await?;
        let next_model = model.clone();
        let turn_active = self
            .inner
            .state
            .with(move |state| {
                if state.model == next_model {
                    return None;
                }
                state.model = next_model;
                Some(state.turn_active)
            })
            .await;
        let Some(turn_active) = turn_active else {
            return Ok(());
        };
        self.launch.model = model;
        if turn_active {
            self.relaunch_before_next_turn = true;
            return Ok(());
        }
        self.relaunch(process).await
    }

    async fn relaunch(&mut self, process: &mut AgyProcess) -> Result<(), String> {
        self.relaunch_before_next_turn = false;
        let conversation_id = process.conversation_id.clone();
        let replacement = AgyProcess::start(
            &self.launch,
            Some(&conversation_id),
            Arc::clone(&self.inner.emitter),
        )
        .await?;
        process.terminate().await;
        *process = replacement;
        #[cfg(feature = "test-support")]
        {
            let pid = process.child.id();
            self.inner
                .state
                .with(move |state| state.native_pid = pid)
                .await;
        }
        Ok(())
    }

    async fn shutdown(mut self, mut process: AgyProcess) {
        self.inner.state.with(|state| state.closing = true).await;
        self.native_children.abort_all();
        while self.native_children.join_next().await.is_some() {}
        process.terminate().await;
        // Nothing to unregister: the shared config holds only the static,
        // credential-free bridge entry. Dropping the descriptor directory
        // removes this session's endpoints and bearer tokens from disk.
        drop(self.descriptor_dir);
        drop(self.skill_projection.take());
        self.inner.emitter.close(
            self.native_child_failure
                .as_deref()
                .unwrap_or("Antigravity backend shut down"),
        );
        let _ = self.shutdown_complete.send(());
    }
}

/// Reports a completion in the same shape as the request that opened it.
///
/// The card's footer is rendered from the typed result — `+A -B` for a diff, an
/// exit code for a command — so a completion that falls back to `Other` leaves
/// a finished tool with no summary of what it did.
fn tool_execution_result(
    tool_type: &ToolRequestType,
    output: &str,
    transcript_result: Option<&str>,
    is_mcp_dispatch: bool,
) -> ToolExecutionResult {
    // An MCP tool's body is whatever that server returned, and the shared
    // agent-control projection reads Tyde's own tools out of it. It recognises
    // a result that is the provider's own payload — including a string holding
    // JSON — but not a `{"output": ...}` envelope invented here, so an MCP
    // result is passed through verbatim rather than wrapped.
    if is_mcp_dispatch {
        return ToolExecutionResult::Other {
            result: Value::String(output.to_string()),
        };
    }
    match tool_type {
        ToolRequestType::ModifyFile { before, after, .. } => {
            let (lines_added, lines_removed) = crate::backend::estimate_line_delta(before, after);
            ToolExecutionResult::ModifyFile {
                lines_added,
                lines_removed,
            }
        }
        ToolRequestType::RunCommand { .. } => ToolExecutionResult::RunCommand {
            // The stream reports a command's output but never its status. The
            // transcript records "The command exited with code N", which is the
            // only place a non-zero exit is stated, so a failing command does
            // not render as a card that looks like it succeeded.
            exit_code: transcript_result
                .and_then(exit_code_from_result)
                .unwrap_or(0),
            stdout: output.to_string(),
            stderr: String::new(),
        },
        ToolRequestType::ReadFiles { file_paths } => ToolExecutionResult::ReadFiles {
            // Native read output does not report a structured size. Host
            // metadata is neither the private runtime nor the file as read.
            files: file_paths
                .iter()
                .map(|path| protocol::FileInfo {
                    path: path.clone(),
                    bytes: None,
                })
                .collect(),
        },
        ToolRequestType::WebSearch { .. } => ToolExecutionResult::WebSearch,
        ToolRequestType::Sleep { .. } => ToolExecutionResult::Sleep,
        ToolRequestType::GenerateImage { .. } => ToolExecutionResult::GenerateImage {
            revised_prompt: None,
            image_count: 1,
        },
        ToolRequestType::TydeSendAgentMessage { .. } => ToolExecutionResult::TydeSendAgentMessage,
        // Headless `agy` answers its own questions. Saying so is the whole
        // value of the card: the alternative is a question card that looks
        // like it is still waiting for the user who never saw it.
        ToolRequestType::AskUserQuestion { .. } => ToolExecutionResult::Other {
            result: json!({
                "answers": transcript_result.map(answers_from_result).unwrap_or_default(),
            }),
        },
        _ => ToolExecutionResult::Other {
            result: json!({ "output": output }),
        },
    }
}

/// What a CLI-answered command printed: the structured `command_result`
/// payload when the command has one, the `result` frame's response text, and
/// the error the CLI reported, if any.
#[derive(Default)]
struct AgyCommandOutput {
    command: Option<Value>,
    response: String,
    error: Option<String>,
}

/// Runs a CLI-answered command (`/help`, `/usage`, …) as its own `--print`
/// invocation. `agy` answers these itself, without a model request, and only
/// in print mode: the stream-json process refuses them with "run it as its own
/// --print /help invocation".
async fn run_antigravity_cli_command(
    program: &str,
    model: Option<&str>,
    cwd: Option<&Path>,
    command: &str,
) -> Result<AgyCommandOutput, String> {
    let mut process = crate::backend::subprocess::command(program)
        .map_err(|err| format!("Antigravity CLI is unavailable: {err:?}"))?;
    process.args(["--output-format", "stream-json", "-p", command]);
    if let Some(model) = model {
        process.args(["--model", model]);
    }
    if let Some(cwd) = cwd {
        process.current_dir(cwd);
    }
    process
        .stdin(crate::backend::subprocess::ProcessStdio::Null)
        .stdout(crate::backend::subprocess::ProcessStdio::Piped)
        .stderr(crate::backend::subprocess::ProcessStdio::Null);
    let output = tokio::time::timeout(ANTIGRAVITY_COMMAND_TIMEOUT, process.output())
        .await
        .map_err(|_| {
            format!(
                "Antigravity did not answer {command} within {}s",
                ANTIGRAVITY_COMMAND_TIMEOUT.as_secs()
            )
        })?
        .map_err(|err| format!("Antigravity could not run {command}: {err}"))?;
    if !output.status.success() {
        return Err(format!(
            "Antigravity {command} failed with {}",
            output.status
        ));
    }
    let mut answer = AgyCommandOutput::default();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let Ok(frame) = parse_frame(line.trim()) else {
            continue;
        };
        match frame {
            AgyFrame::CommandResult(payload) => answer.command = Some(payload),
            AgyFrame::Result(result) => {
                answer.error = result.status.eq_ignore_ascii_case("ERROR").then(|| {
                    result
                        .error
                        .clone()
                        .filter(|error| !error.trim().is_empty())
                        .unwrap_or_else(|| format!("Antigravity reported {command} failed"))
                });
                answer.response = result.response;
            }
            _ => {}
        }
    }
    Ok(answer)
}

/// The commands the CLI itself answers, read from its own `/help`, minus the
/// configuration commands Tyde owns per session.
async fn read_antigravity_slash_commands(model: String) -> Option<protocol::SlashCommandCatalog> {
    let output = match run_antigravity_cli_command("agy", Some(&model), None, "/help").await {
        Ok(output) => output,
        Err(error) => {
            tracing::warn!(%error, "Antigravity /help failed; slash commands unavailable");
            return None;
        }
    };
    if let Some(error) = output.error {
        tracing::warn!(
            error,
            "Antigravity /help reported an error; slash commands unavailable"
        );
        return None;
    }
    let commands = output
        .command
        .as_ref()?
        .get("data")?
        .get("commands")?
        .as_array()?
        .iter()
        .filter_map(|command| {
            let name = command.get("name")?.as_str()?.trim();
            if name.is_empty() || ANTIGRAVITY_CONFIGURATION_COMMANDS.contains(&name) {
                return None;
            }
            Some(protocol::SlashCommand {
                name: name.to_owned(),
                description: command
                    .get("description")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|description| !description.is_empty())
                    .map(str::to_owned),
                input_hint: None,
            })
        })
        .collect::<Vec<_>>();
    (!commands.is_empty()).then_some(protocol::SlashCommandCatalog { commands })
}

/// Reads the account's remaining quota with no conversation and no live agent.
///
/// `/usage` is answered by print mode without a model request or selection.
pub(crate) async fn read_capacity_out_of_band() -> protocol::BackendCapacityState {
    match read_antigravity_capacity().await {
        Ok(report) => protocol::BackendCapacityState::Known { report },
        Err(reason) => protocol::BackendCapacityState::Unavailable { reason },
    }
}

/// Reads the account's remaining quota without starting a turn.
///
/// `/usage` is answered by print mode directly — no model request, no turn,
/// zero tokens — so this costs a short-lived process and nothing else.
async fn read_antigravity_capacity() -> Result<CapacityReport, CapacityUnavailableReason> {
    let mut command = crate::backend::subprocess::command("agy")
        .map_err(|_| CapacityUnavailableReason::SourceUnreachable)?;
    command.args(["--output-format", "stream-json", "-p", "/usage"]);
    command
        .stdin(crate::backend::subprocess::ProcessStdio::Null)
        .stdout(crate::backend::subprocess::ProcessStdio::Piped)
        .stderr(crate::backend::subprocess::ProcessStdio::Null)
        // A timeout drops the `output()` future, which does not reap the child
        // unless it is killed on drop. Without this a hung `agy` outlives every
        // probe that gives up on it, and the poll runs on a timer.
        ;

    let output = tokio::time::timeout(ANTIGRAVITY_CAPACITY_TIMEOUT, command.output())
        .await
        .map_err(|_| CapacityUnavailableReason::SourceTimedOut)?
        .map_err(|_| CapacityUnavailableReason::SourceUnreachable)?;
    if !output.status.success() {
        return Err(CapacityUnavailableReason::SourceUnreachable);
    }
    let report = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| parse_frame(line.trim()).ok())
        .find_map(|frame| usage_report_from_frame(&frame))
        .ok_or(CapacityUnavailableReason::MalformedReport)?;
    map_antigravity_capacity(&report)
}

fn map_antigravity_capacity(
    report: &AgyUsageReport,
) -> Result<CapacityReport, CapacityUnavailableReason> {
    let buckets = report
        .groups
        .iter()
        .flat_map(|group| {
            group
                .buckets
                .iter()
                .map(|bucket| antigravity_capacity_bucket(&group.name, bucket))
        })
        .collect::<Vec<_>>();
    if buckets.is_empty() {
        return Err(CapacityUnavailableReason::MalformedReport);
    }
    Ok(CapacityReport {
        source: CapacitySource::AntigravityUsageCommand,
        observed_at_ms: Some(now_ms()),
        plan: None,
        buckets,
        coverage: CapacityCoverage::AllVendorBuckets,
    })
}

fn antigravity_capacity_bucket(group: &str, bucket: &AgyUsageBucket) -> CapacityBucket {
    // `agy` reports how much is *left*, so the used figure is its complement
    // rather than a number the vendor stated. `ValueProvenance` describes the
    // used value, which is why this is not `vendor_reported`.
    let measure = match bucket.remaining_fraction {
        Some(remaining) => {
            let remaining_percent = (remaining * 100.0).round().clamp(0.0, 100.0) as u8;
            CapacityMeasure::UsedPercent {
                used_percent: 100_u8.saturating_sub(remaining_percent),
                remaining_percent,
                provenance: ValueProvenance {
                    vendor_reported: false,
                },
            }
        }
        None => CapacityMeasure::ReportedWithoutMagnitude,
    };
    CapacityBucket {
        id: CapacityBucketId::Antigravity {
            bucket: bucket.id.clone(),
        },
        label: bucket.name.clone(),
        measure,
        // A group is a set of models sharing one limit — "Gemini Models",
        // "Claude and GPT models" — which is what a model family is.
        scope: CapacityScope::ModelFamily {
            name: group.to_string(),
        },
        window: match bucket.window.as_str() {
            "weekly" => CapacityWindow::Rolling {
                duration_minutes: 7 * 24 * 60,
            },
            "5h" => CapacityWindow::Rolling {
                duration_minutes: 5 * 60,
            },
            _ => CapacityWindow::NotReported,
        },
        reset: bucket
            .reset_time
            .as_deref()
            .and_then(|stamp| chrono::DateTime::parse_from_rfc3339(stamp).ok())
            .map(|stamp| CapacityReset::At {
                at_ms: stamp.timestamp_millis().max(0) as u64,
            })
            .unwrap_or(CapacityReset::NotReported),
        status: None,
    }
}

/// The conversations `agy` can resume, read off its own store.
///
/// `agy` exposes no way to list them: `/resume` is interactive and print mode
/// answers no equivalent command. The store is the authority anyway — resuming
/// requires `<id>.db` to exist, which is exactly what
/// `ensure_antigravity_conversation_exists` checks — so this enumerates it
/// directly and enriches each entry from the transcript beside it.
#[derive(Clone, PartialEq, prost::Message)]
struct AntigravityTrajectoryMetadata {
    #[prost(message, repeated, tag = "1")]
    workspaces: Vec<AntigravityWorkspace>,
    #[prost(string, repeated, tag = "7")]
    workspace_uris: Vec<String>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct AntigravityWorkspace {
    #[prost(string, tag = "1")]
    uri: String,
}

fn replace_antigravity_metadata(
    path: &Path,
    expected: &[u8],
    replacement: &[u8],
) -> Result<(), String> {
    let mut connection =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)
            .map_err(|error| format!("Cannot open native conversation metadata: {error}"))?;
    let transaction = connection
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|error| format!("Cannot lock native conversation metadata: {error}"))?;
    let changed = transaction
        .execute(
            "UPDATE trajectory_metadata_blob SET data = ?1 WHERE id = 'main' AND data = ?2",
            rusqlite::params![replacement, expected],
        )
        .map_err(|error| {
            format!("Cannot update native conversation workspace metadata: {error}")
        })?;
    if changed != 1 {
        return Err(
            "Native conversation metadata changed concurrently; relocation cannot proceed"
                .to_owned(),
        );
    }
    transaction
        .commit()
        .map_err(|error| format!("Cannot commit native conversation workspace metadata: {error}"))
}

fn relocate_antigravity_metadata(
    path: &Path,
    previous_root: &str,
    roots: &[String],
) -> Result<(Vec<u8>, Vec<u8>), String> {
    use prost::Message;
    let connection =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|error| format!("Cannot read native conversation metadata: {error}"))?;
    let before: Vec<u8> = connection
        .query_row(
            "SELECT data FROM trajectory_metadata_blob WHERE id = 'main'",
            [],
            |row| row.get(0),
        )
        .map_err(|error| format!("Cannot read native workspace metadata: {error}"))?;
    drop(connection);
    let stored = AntigravityTrajectoryMetadata::decode(before.as_slice())
        .map_err(|error| format!("Invalid native conversation workspace metadata: {error}"))?;
    let previous_uri = url::Url::from_file_path(previous_root)
        .map_err(|()| "Invalid previous workspace path".to_owned())?
        .to_string();
    let stored_primary = stored
        .workspace_uris
        .first()
        .or_else(|| stored.workspaces.first().map(|workspace| &workspace.uri));
    if stored_primary != Some(&previous_uri) {
        return Err(
            "Native conversation workspace differs from this runtime; refusing a stale relocation"
                .to_owned(),
        );
    }
    let uris = roots
        .iter()
        .map(|root| {
            url::Url::from_file_path(root)
                .map(|uri| uri.to_string())
                .map_err(|()| format!("Cannot encode workspace URI: {root}"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let metadata = AntigravityTrajectoryMetadata {
        workspaces: uris
            .iter()
            .map(|uri| AntigravityWorkspace { uri: uri.clone() })
            .collect(),
        workspace_uris: uris,
    };
    let mut after = metadata.encode_to_vec();
    let mut remaining = before.as_slice();
    // Retain every unknown field byte-for-byte; decoding/re-encoding a partial
    // schema would destroy provider-owned history and configuration metadata.
    while !remaining.is_empty() {
        let field = remaining;
        let (tag, wire) = prost::encoding::decode_key(&mut remaining)
            .map_err(|error| format!("Invalid native workspace metadata: {error}"))?;
        prost::encoding::skip_field(
            wire,
            tag,
            &mut remaining,
            prost::encoding::DecodeContext::default(),
        )
        .map_err(|error| format!("Invalid native workspace metadata field: {error}"))?;
        if tag != 1 && tag != 7 {
            after.extend_from_slice(&field[..field.len() - remaining.len()]);
        }
    }
    replace_antigravity_metadata(path, &before, &after)?;
    tracing::info!(
        root_count = roots.len(),
        "Updated native Antigravity trajectory workspace fields without changing history"
    );
    Ok((before, after))
}

fn antigravity_session_workspace_roots(path: &Path) -> Result<Vec<String>, String> {
    use prost::Message;
    let connection =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|error| {
                format!(
                    "Cannot open Antigravity session {}: {error}",
                    path.display()
                )
            })?;
    let bytes: Vec<u8> = connection
        .query_row(
            "SELECT data FROM trajectory_metadata_blob WHERE id = 'main'",
            [],
            |row| row.get(0),
        )
        .map_err(|error| {
            format!(
                "Cannot read Antigravity workspace metadata {}: {error}",
                path.display()
            )
        })?;
    let metadata = AntigravityTrajectoryMetadata::decode(bytes.as_slice()).map_err(|error| {
        format!(
            "Invalid Antigravity workspace metadata {}: {error}",
            path.display()
        )
    })?;
    metadata
        .workspaces
        .into_iter()
        .map(|workspace| {
            let uri = url::Url::parse(&workspace.uri)
                .map_err(|error| format!("Invalid Antigravity workspace URI: {error}"))?;
            let path = uri.to_file_path().map_err(|()| {
                format!(
                    "Antigravity workspace is not a local file URI: {}",
                    workspace.uri
                )
            })?;
            path.into_os_string()
                .into_string()
                .map_err(|_| "Antigravity workspace path is not UTF-8".to_owned())
        })
        .collect()
}

fn list_antigravity_sessions(
    conversations_dir: &Path,
    brain_dir: &Path,
) -> Result<Vec<BackendSession>, String> {
    let Ok(entries) = fs::read_dir(conversations_dir) else {
        return Ok(Vec::new());
    };
    let mut sessions = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            // `.db-wal` and `.db-shm` sit beside the store and are not
            // conversations; a non-UUID stem is not one either.
            if path.extension().and_then(|ext| ext.to_str()) != Some("db") {
                return None;
            }
            let id = path.file_stem().and_then(|stem| stem.to_str())?;
            let session_id = SessionId(id.to_string());
            if !is_antigravity_native_session_id(&session_id) {
                return None;
            }
            let metadata = entry.metadata().ok();
            let updated_at_ms = metadata
                .as_ref()
                .and_then(|metadata| metadata.modified().ok())
                .and_then(system_time_to_ms);
            let opening = antigravity_transcript_opening(brain_dir, id);
            Some(
                antigravity_session_workspace_roots(&path).map(|workspace_roots| BackendSession {
                    id: session_id,
                    backend_kind: BackendKind::Antigravity,
                    workspace_roots,
                    title: opening.as_ref().and_then(|opening| opening.title.clone()),
                    token_count: None,
                    created_at_ms: opening.as_ref().and_then(|opening| opening.created_at_ms),
                    updated_at_ms,
                    resumable: true,
                }),
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    // Most recently touched first, which is the order a resume picker wants.
    sessions.sort_by_key(|session| std::cmp::Reverse(session.updated_at_ms));
    Ok(sessions)
}

struct AntigravityTranscriptOpening {
    title: Option<String>,
    created_at_ms: Option<u64>,
}

// Native children have a trajectory DB, not the CLI's rendered JSONL log.
// These fields are from agy's trajectory.proto and cortex.proto store schema.
#[derive(Clone, PartialEq, prost::Message)]
struct NativeAgyStep {
    #[prost(int32, tag = "1")]
    kind: i32,
    #[prost(int32, tag = "4")]
    status: i32,
    #[prost(message, optional, tag = "5")]
    metadata: Option<NativeAgyMetadata>,
    #[prost(message, optional, tag = "20")]
    response: Option<NativeAgyResponse>,
    #[prost(message, optional, tag = "140")]
    generic: Option<NativeAgyGeneric>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct NativeAgyMetadata {
    #[prost(message, optional, tag = "1")]
    created_at: Option<NativeAgyTimestamp>,
    #[prost(message, optional, tag = "4")]
    tool_call: Option<NativeAgyToolCall>,
    #[prost(message, optional, tag = "9")]
    usage: Option<NativeAgyUsage>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct NativeAgyTimestamp {
    #[prost(int64, tag = "1")]
    seconds: i64,
    #[prost(int32, tag = "2")]
    nanos: i32,
}

#[derive(Clone, PartialEq, prost::Message)]
struct NativeAgyUsage {
    #[prost(uint64, tag = "2")]
    input: u64,
    #[prost(uint64, tag = "3")]
    output: u64,
    #[prost(uint64, tag = "4")]
    cache_write: u64,
    #[prost(uint64, tag = "5")]
    cache_read: u64,
    #[prost(uint64, tag = "9")]
    thinking: u64,
}

#[derive(Clone, PartialEq, prost::Message)]
struct NativeAgyResponse {
    #[prost(string, tag = "1")]
    text: String,
    #[prost(string, tag = "3")]
    thinking: String,
    #[prost(string, tag = "6")]
    message_id: String,
    #[prost(message, repeated, tag = "7")]
    tools: Vec<NativeAgyToolCall>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct NativeAgyToolCall {
    #[prost(string, tag = "1")]
    id: String,
    #[prost(string, tag = "2")]
    name: String,
    #[prost(string, tag = "3")]
    arguments: String,
}

#[derive(Clone, PartialEq, prost::Message)]
struct NativeAgyGeneric {
    #[prost(message, optional, tag = "2")]
    result: Option<NativeAgyGenericResult>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct NativeAgyGenericResult {
    #[prost(string, tag = "1")]
    text: String,
}

#[derive(Clone, PartialEq, prost::Message)]
struct NativeAgyExecutor {
    #[prost(enumeration = "NativeAgyTerminationReason", tag = "1")]
    termination_reason: i32,
    #[prost(bool, tag = "4")]
    proceeded_with_auto_continue: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, prost::Enumeration)]
#[repr(i32)]
enum NativeAgyTerminationReason {
    Unspecified = 0,
    Error = 1,
    UserCanceled = 2,
    MaxInvocations = 3,
    NoToolCall = 4,
    MaxForcedInvocations = 6,
    EarlyContinue = 7,
    TerminalStepType = 8,
    TerminalCustomHook = 9,
    InjectedResponse = 10,
    MaxTokenBudgetExceeded = 11,
}

enum NativeAgyChildCompletion {
    Completed(Vec<ChatEvent>),
    Failed {
        events: Vec<ChatEvent>,
        reason: NativeAgyTerminationReason,
    },
}

async fn wait_for_antigravity_native_child(
    path: &Path,
) -> Result<NativeAgyChildCompletion, String> {
    use notify::Watcher;
    let (updates, mut changed) = mpsc::unbounded_channel();
    let mut watcher = notify::recommended_watcher(move |event| {
        let _ = updates.send(event);
    })
    .map_err(|error| format!("Cannot observe native child completion: {error}"))?;
    let directory = path
        .parent()
        .ok_or("Native child trajectory has no directory")?;
    watcher
        .watch(directory, notify::RecursiveMode::NonRecursive)
        .map_err(|error| format!("Cannot subscribe to native child completion: {error}"))?;
    let wal = path.with_extension("db-wal");
    loop {
        if let Some(events) = antigravity_native_child_history(path)? {
            return Ok(events);
        }
        loop {
            let event: notify::Result<notify::Event> = changed
                .recv()
                .await
                .ok_or("Native child completion observer closed")?;
            let event = event
                .map_err(|error| format!("Native child completion observation failed: {error}"))?;
            if matches!(
                event.kind,
                notify::EventKind::Modify(_)
                    | notify::EventKind::Create(_)
                    | notify::EventKind::Remove(_)
            ) && event
                .paths
                .iter()
                .any(|entry| entry == path || entry == &wal)
            {
                break;
            }
        }
    }
}

fn antigravity_native_child_history(
    path: &Path,
) -> Result<Option<NativeAgyChildCompletion>, String> {
    use prost::Message;
    use protocol::{
        ChatMessage, ChatMessageId, MessageSender, ReasoningData, ToolExecutionCompletedData,
        ToolRequest,
    };
    let connection =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|_| "Cannot open Antigravity native child trajectory".to_owned())?;
    use rusqlite::OptionalExtension;
    let executor = connection
        .query_row(
            "SELECT data FROM executor_metadata ORDER BY idx DESC LIMIT 1",
            [],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .optional()
        .map_err(|error| format!("Cannot read native child execution status: {error}"))?;
    let Some(executor) = executor else {
        return Ok(None);
    };
    let executor = NativeAgyExecutor::decode(executor.as_slice())
        .map_err(|_| "Invalid native child execution status".to_owned())?;
    let reason = NativeAgyTerminationReason::try_from(executor.termination_reason)
        .map_err(|_| "Unsupported Antigravity native child termination reason".to_owned())?;
    if executor.proceeded_with_auto_continue {
        return Ok(None);
    }
    let failed = match reason {
        NativeAgyTerminationReason::Unspecified | NativeAgyTerminationReason::EarlyContinue => {
            return Ok(None);
        }
        NativeAgyTerminationReason::Error
        | NativeAgyTerminationReason::UserCanceled
        | NativeAgyTerminationReason::MaxInvocations
        | NativeAgyTerminationReason::MaxForcedInvocations
        | NativeAgyTerminationReason::MaxTokenBudgetExceeded => true,
        NativeAgyTerminationReason::NoToolCall
        | NativeAgyTerminationReason::TerminalStepType
        | NativeAgyTerminationReason::TerminalCustomHook
        | NativeAgyTerminationReason::InjectedResponse => false,
    };
    let mut query = connection
        .prepare("SELECT step_format, step_payload FROM steps ORDER BY idx")
        .map_err(|error| format!("Cannot read native child steps: {error}"))?;
    let rows = query
        .query_map([], |row| {
            Ok((row.get::<_, i32>(0)?, row.get::<_, Vec<u8>>(1)?))
        })
        .map_err(|error| format!("Cannot enumerate native child steps: {error}"))?;
    let mut events = Vec::new();
    let mut pending = BTreeMap::new();
    for row in rows {
        let (format, payload) =
            row.map_err(|error| format!("Cannot read native child step: {error}"))?;
        if format != 0 {
            return Err("Unsupported Antigravity native child step encoding".to_owned());
        }
        let step = NativeAgyStep::decode(payload.as_slice())
            .map_err(|_| "Invalid Antigravity native child step".to_owned())?;
        // System context is not a user-visible assistant response.
        if step.kind == 101 {
            continue;
        }
        let metadata = step.metadata.ok_or("Native child step omitted metadata")?;
        match step.kind {
            15 => {
                if step.status != 3 {
                    return Err("Native child response was not completed".to_owned());
                }
                let response = step
                    .response
                    .ok_or("Native child response omitted payload")?;
                if response.message_id.is_empty() {
                    return Err("Native child response omitted identity".to_owned());
                }
                let mut declarations = Vec::new();
                let mut requests = Vec::new();
                for call in response.tools {
                    if call.id.is_empty() || call.name.is_empty() {
                        return Err("Native child tool omitted identity".to_owned());
                    }
                    let args: Value = serde_json::from_str(&call.arguments)
                        .map_err(|_| "Native child tool contained invalid arguments".to_owned())?;
                    let tool_type = tool_request_type(&call.name, &args, true);
                    let is_mcp = call.name == MCP_DISPATCH_TOOL;
                    let (name, args) = if is_mcp {
                        mcp_inner_call(&args)
                    } else {
                        (call.name, args)
                    };
                    if pending
                        .insert(call.id.clone(), (tool_type.clone(), is_mcp))
                        .is_some()
                    {
                        return Err("Native child repeated a tool identity".to_owned());
                    }
                    declarations.push(ToolUseData {
                        tool_call_id: call.id.clone(),
                        name: name.clone(),
                        arguments: args,
                        content_offset: None,
                    });
                    requests.push(ChatEvent::ToolRequest(ToolRequest {
                        tool_call_id: call.id,
                        tool_name: name,
                        tool_type,
                    }));
                }
                events.push(ChatEvent::MessageAdded(ChatMessage {
                    origin: None,
                    message_id: Some(ChatMessageId(response.message_id)),
                    timestamp: metadata
                        .created_at
                        .map(|stamp| {
                            (stamp.seconds.max(0) as u64)
                                .saturating_mul(1000)
                                .saturating_add(stamp.nanos.max(0) as u64 / 1_000_000)
                        })
                        .unwrap_or_default(),
                    sender: MessageSender::Assistant {
                        agent: ANTIGRAVITY_AGENT_NAME.to_owned(),
                    },
                    content: response.text,
                    reasoning: (!response.thinking.is_empty()).then_some(ReasoningData {
                        text: response.thinking,
                        tokens: metadata.usage.as_ref().map(|usage| usage.thinking),
                        signature: None,
                        blob: None,
                    }),
                    tool_calls: declarations,
                    model_info: None,
                    token_usage: metadata.usage.map(|usage| {
                        MessageTokenUsage::request_known(protocol::TokenUsage {
                            input_tokens: usage.input,
                            output_tokens: usage.output,
                            total_tokens: usage
                                .input
                                .saturating_add(usage.output)
                                .saturating_add(usage.cache_read)
                                .saturating_add(usage.cache_write),
                            cached_prompt_tokens: Some(usage.cache_read),
                            cache_creation_input_tokens: Some(usage.cache_write),
                            reasoning_tokens: Some(usage.thinking),
                        })
                    }),
                    context_breakdown: None,
                    images: None,
                }));
                events.extend(requests);
            }
            132 => {
                let call = metadata
                    .tool_call
                    .ok_or("Native child tool result omitted identity")?;
                let (tool_type, is_mcp) = pending
                    .remove(&call.id)
                    .ok_or("Native child result has no declared tool")?;
                let result = step
                    .generic
                    .and_then(|generic| generic.result)
                    .ok_or("Native child tool result omitted payload")?;
                let outcome = match step.status {
                    3 => ToolExecutionOutcome::Succeeded {
                        result: tool_execution_result(
                            &tool_type,
                            &result.text,
                            Some(&result.text),
                            is_mcp,
                        ),
                    },
                    7 => ToolExecutionOutcome::Failed {
                        message: result.text,
                        details: None,
                        normalization_failure: None,
                        result: None,
                    },
                    6 | 12 => ToolExecutionOutcome::Cancelled {
                        message: result.text,
                    },
                    _ => return Err("Native child tool has no terminal status".to_owned()),
                };
                events.push(ChatEvent::ToolExecutionCompleted(
                    ToolExecutionCompletedData {
                        tool_call_id: call.id,
                        outcome,
                    },
                ));
            }
            _ => {
                return Err(format!(
                    "Unsupported Antigravity native child step kind {}",
                    step.kind
                ));
            }
        }
    }
    if failed {
        for (tool_call_id, _) in pending {
            let message = format!("Native child executor terminated: {reason:?}");
            events.push(ChatEvent::ToolExecutionCompleted(
                ToolExecutionCompletedData {
                    tool_call_id,
                    outcome: if reason == NativeAgyTerminationReason::UserCanceled {
                        ToolExecutionOutcome::Cancelled { message }
                    } else {
                        ToolExecutionOutcome::Failed {
                            message,
                            details: None,
                            normalization_failure: None,
                            result: None,
                        }
                    },
                },
            ));
        }
        return Ok(Some(NativeAgyChildCompletion::Failed { events, reason }));
    }
    if !pending.is_empty() || events.is_empty() {
        return Err("Native child trajectory has no complete public response".to_owned());
    }
    Ok(Some(NativeAgyChildCompletion::Completed(events)))
}

struct AgyNativeChildRelay {
    child_db: PathBuf,
    tool_call_id: String,
    name: String,
    child: crate::backend::SubAgentHandle,
    emitter: Arc<TurnEmitter>,
    session_id: SessionId,
    description: String,
    subagents: Arc<dyn SubAgentEmitter>,
}

impl AgyNativeChildRelay {
    async fn run(self) -> Result<(), String> {
        let Self {
            child_db,
            tool_call_id,
            name,
            mut child,
            emitter,
            session_id,
            description,
            subagents,
        } = self;
        use protocol::{
            MessageSender, StreamEndData, StreamStartData, SubAgentProgress,
            SubAgentProgressStatus, ToolExecutionMode, ToolProgressData, ToolProgressUpdate,
        };
        let (events, failure) = match wait_for_antigravity_native_child(&child_db).await? {
            NativeAgyChildCompletion::Completed(events) => (events, None),
            NativeAgyChildCompletion::Failed { events, reason } => (events, Some(reason)),
        };
        if child.event_tx.is_closed() {
            child = subagents
                .on_subagent_spawned(
                    tool_call_id.clone(),
                    name.clone(),
                    description,
                    ANTIGRAVITY_AGENT_NAME.to_owned(),
                    Some(session_id),
                )
                .await?;
        }
        child
            .event_tx
            .send(ChatEvent::TypingStatusChanged(true))
            .map_err(|_| "Native child relay closed before response delivery".to_owned())?;
        let tool_calls = events
            .iter()
            .filter(|event| matches!(event, ChatEvent::ToolRequest(_)))
            .count() as u64;
        for event in events {
            match event {
                ChatEvent::MessageAdded(message)
                    if matches!(message.sender, MessageSender::Assistant { .. }) =>
                {
                    child
                        .event_tx
                        .send(ChatEvent::StreamStart(StreamStartData {
                            agent: ANTIGRAVITY_AGENT_NAME.to_owned(),
                            model: message.model_info.as_ref().map(|info| info.model.clone()),
                        }))
                        .map_err(|_| {
                            "Native child relay closed during response delivery".to_owned()
                        })?;
                    child
                        .event_tx
                        .send(ChatEvent::StreamEnd(StreamEndData { message }))
                        .map_err(|_| {
                            "Native child relay closed during response delivery".to_owned()
                        })?;
                }
                event => {
                    child.event_tx.send(event).map_err(|_| {
                        "Native child relay closed during response delivery".to_owned()
                    })?;
                }
            }
        }
        if let Some(reason) = failure {
            let error = format!("Antigravity native child executor terminated: {reason:?}");
            child
                .event_tx
                .send(ChatEvent::MessageAdded(protocol::ChatMessage {
                    origin: None,
                    message_id: None,
                    timestamp: crate::agent::now_ms(),
                    sender: MessageSender::Error,
                    content: error.clone(),
                    reasoning: None,
                    tool_calls: Vec::new(),
                    model_info: None,
                    token_usage: None,
                    context_breakdown: None,
                    images: None,
                }))
                .map_err(|_| "Native child relay closed before failure delivery".to_owned())?;
            emitter.backend_error(&error);
        }
        child
            .event_tx
            .send(ChatEvent::TypingStatusChanged(false))
            .map_err(|_| "Native child relay closed before completion delivery".to_owned())?;
        emitter.tool_progress(&ToolProgressData {
            tool_call_id,
            execution_mode: ToolExecutionMode::Background,
            cancellable: false,
            update: ToolProgressUpdate::SubAgent(SubAgentProgress {
                backend_kind: None,
                agent_id: child.agent_id,
                agent_name: name,
                last_tool_name: None,
                tool_calls,
                completed: true,
                status: match failure {
                    Some(NativeAgyTerminationReason::UserCanceled) => {
                        SubAgentProgressStatus::Stopped
                    }
                    Some(_) => SubAgentProgressStatus::Failed,
                    None => SubAgentProgressStatus::Completed,
                },
            }),
        });
        Ok(())
    }
}

fn antigravity_history(brain_dir: &Path, session_id: &str) -> Result<Vec<ChatEvent>, String> {
    use protocol::{
        ChatMessage, ChatMessageId, MessageSender, ReasoningData, ToolExecutionCompletedData,
        ToolRequest,
    };
    let path = TranscriptReader::new(brain_dir, session_id)
        .path()
        .to_path_buf();
    let text = fs::read_to_string(&path).map_err(|error| {
        format!(
            "Cannot read Antigravity history {}: {error}",
            path.display()
        )
    })?;
    let mut events = Vec::new();
    let mut pending = BTreeMap::new();
    let mut turn = 0;
    // Native JSONL appends can be observed before the final record is flushed.
    let complete = text
        .rsplit_once('\n')
        .map(|(complete, _)| complete)
        .unwrap_or("");
    for line in complete.lines().filter(|line| !line.trim().is_empty()) {
        let row: Value = serde_json::from_str(line)
            .map_err(|error| format!("Invalid Antigravity history {}: {error}", path.display()))?;
        let index = row
            .get("step_index")
            .and_then(Value::as_u64)
            .ok_or("Antigravity history omitted step_index")?;
        let kind = row.get("type").and_then(Value::as_str).unwrap_or_default();
        let content = row
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if kind == "GENERIC" {
            if let Some((tool_call_id, tool_type, is_mcp)) = pending.remove(&index) {
                let outcome = if row.get("status").and_then(Value::as_str) == Some("ERROR") {
                    ToolExecutionOutcome::Failed {
                        message: content.to_owned(),
                        details: None,
                        normalization_failure: None,
                        result: None,
                    }
                } else {
                    let output = content
                        .lines()
                        .skip_while(|line| {
                            line.starts_with("Created At:") || line.starts_with("Completed At:")
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    ToolExecutionOutcome::Succeeded {
                        result: tool_execution_result(&tool_type, &output, Some(content), is_mcp),
                    }
                };
                events.push(ChatEvent::ToolExecutionCompleted(
                    ToolExecutionCompletedData {
                        tool_call_id,
                        outcome,
                    },
                ));
            }
            continue;
        }
        let sender = match kind {
            "USER_INPUT" => {
                turn += 1;
                MessageSender::User
            }
            "PLANNER_RESPONSE" => MessageSender::Assistant {
                agent: ANTIGRAVITY_AGENT_NAME.to_owned(),
            },
            _ => continue,
        };
        let content = if kind == "USER_INPUT" {
            content
                .split_once("<USER_REQUEST>")
                .and_then(|(_, rest)| rest.split_once("</USER_REQUEST>"))
                .map(|(request, _)| request.trim())
                .unwrap_or(content)
        } else {
            content
        };
        let content = if kind == "USER_INPUT" {
            super::workspace_prompt_user_text(content)
        } else {
            content
        };
        let mut declarations = Vec::new();
        let mut requests = Vec::new();
        if let Some(calls) = row.get("tool_calls").and_then(Value::as_array) {
            for (offset, call) in calls.iter().enumerate() {
                let name = call
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or("Antigravity history tool omitted name")?;
                let args = call.get("args").cloned().unwrap_or(Value::Null);
                let tool_index = index + offset as u64 + 1;
                let id = format!("agy-{session_id}-{turn}-{tool_index}");
                let tool_type = tool_request_type(name, &args, true);
                let (name, args) = if name == MCP_DISPATCH_TOOL {
                    mcp_inner_call(&args)
                } else {
                    (name.to_owned(), args)
                };
                pending.insert(
                    tool_index,
                    (
                        id.clone(),
                        tool_type.clone(),
                        call.get("name").and_then(Value::as_str) == Some(MCP_DISPATCH_TOOL),
                    ),
                );
                requests.push(ChatEvent::ToolRequest(ToolRequest {
                    tool_call_id: id.clone(),
                    tool_name: name.clone(),
                    tool_type,
                }));
                declarations.push(ToolUseData {
                    tool_call_id: id,
                    name,
                    arguments: args,
                    content_offset: None,
                });
            }
        }
        if content.is_empty() && declarations.is_empty() {
            continue;
        }
        let reasoning = row
            .get("thinking")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty());
        events.push(ChatEvent::MessageAdded(ChatMessage {
            origin: None,
            message_id: Some(ChatMessageId(format!("agy-{session_id}-step-{index}"))),
            timestamp: row
                .get("created_at")
                .and_then(Value::as_str)
                .and_then(|stamp| chrono::DateTime::parse_from_rfc3339(stamp).ok())
                .map(|stamp| stamp.timestamp_millis().max(0) as u64)
                .unwrap_or_default(),
            sender,
            content: content.to_owned(),
            reasoning: reasoning.map(|text| ReasoningData {
                text: text.to_owned(),
                tokens: None,
                signature: None,
                blob: None,
            }),
            tool_calls: declarations,
            model_info: None,
            token_usage: None,
            context_breakdown: None,
            images: None,
        }));
        events.extend(requests);
    }
    Ok(events)
}

fn antigravity_transcript_opening(
    brain_dir: &Path,
    conversation_id: &str,
) -> Option<AntigravityTranscriptOpening> {
    let path = brain_dir
        .join(conversation_id)
        .join(".system_generated")
        .join("logs")
        .join("transcript_full.jsonl");
    let text = fs::read_to_string(path).ok()?;
    let first = text.lines().find(|line| !line.trim().is_empty())?;
    let value = serde_json::from_str::<Value>(first).ok()?;
    let created_at_ms = value
        .get("created_at")
        .and_then(Value::as_str)
        .and_then(|stamp| chrono::DateTime::parse_from_rfc3339(stamp).ok())
        .map(|stamp| stamp.timestamp_millis().max(0) as u64);
    Some(AntigravityTranscriptOpening {
        title: value
            .get("content")
            .and_then(Value::as_str)
            .and_then(antigravity_title_from_user_input),
        created_at_ms,
    })
}

/// `agy` wraps the prompt it was given in `<USER_REQUEST>` and appends its own
/// metadata blocks, none of which belong in a session's name.
fn antigravity_title_from_user_input(content: &str) -> Option<String> {
    const OPEN: &str = "<USER_REQUEST>";
    const CLOSE: &str = "</USER_REQUEST>";
    let start = content.find(OPEN)? + OPEN.len();
    let end = content[start..].find(CLOSE)? + start;
    let request = content[start..end].trim();
    let first_line = request.lines().find(|line| !line.trim().is_empty())?.trim();
    if first_line.is_empty() {
        return None;
    }
    Some(first_line.chars().take(120).collect())
}

fn system_time_to_ms(time: SystemTime) -> Option<u64> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .map(|elapsed| elapsed.as_millis() as u64)
}

/// Where `agy` keeps the per-conversation transcript the tool cards are
/// enriched from. It is the sibling of the conversations directory, so a caller
/// that redirects one redirects both rather than reading transcripts from a
/// store it is not writing to.
fn antigravity_brain_dir(conversations_dir: &Path) -> PathBuf {
    match conversations_dir.parent() {
        Some(parent) => parent.join("brain"),
        None => conversations_dir.join("brain"),
    }
}

impl Backend for AntigravityBackend {
    fn workspace_relocation_wake(&self) -> Option<tokio::sync::watch::Receiver<()>> {
        Some(self.inner.relocation_wake.subscribe())
    }

    fn native_session_id_is_valid(session_id: &SessionId) -> bool {
        is_antigravity_native_session_id(session_id)
    }

    fn resolve_storage_root(configured: Option<&Path>) -> Result<Option<PathBuf>, String> {
        resolve_antigravity_conversations_dir(configured).map(Some)
    }

    fn stored_session_is_resumable(
        session_id: &SessionId,
        stored_resumable: bool,
        has_parent: bool,
        superseded: bool,
        storage: &crate::backend::BackendStorage,
    ) -> bool {
        if superseded || (!stored_resumable && has_parent) {
            return false;
        }
        resolve_antigravity_conversations_dir(storage.root(BackendKind::Antigravity))
            .is_ok_and(|root| is_antigravity_session_resumable(session_id, &root))
    }

    fn resolve_session_settings(config: &BackendSpawnConfig) -> protocol::SessionSettingsValues {
        resolve_session_settings(config)
    }

    fn startup_drop_cancels_workers() -> bool {
        false
    }

    fn session_is_resumable(
        session_id: &SessionId,
        _workspace_roots: &[String],
        _resolved: &crate::backend::customization::ResolvedSpawnConfig,
    ) -> bool {
        is_antigravity_native_session_id(session_id)
    }

    fn skill_delivery() -> crate::backend::customization::SkillDelivery {
        crate::backend::customization::SkillDelivery::NativeDiscovery
    }

    fn builtin_tier_config() -> settings_model::BackendTierConfig {
        settings_model::BackendTierConfig {
            low: antigravity_cost_hint_defaults(SpawnCostHint::Low),
            high: antigravity_cost_hint_defaults(SpawnCostHint::High),
        }
    }

    async fn read_capacity_out_of_band(
        _context: &crate::backend::BackendProbeContext,
    ) -> protocol::BackendCapacityState {
        read_capacity_out_of_band().await
    }

    fn validate_tool_categories(categories: &[protocol::ToolCategory]) -> Result<(), String> {
        for category in categories {
            match category {
                protocol::ToolCategory::AskUser => {}
                protocol::ToolCategory::AgentDelegation => {
                    return Err("Backend cannot enforce agent-delegation exclusion".into());
                }
            }
        }
        Ok(())
    }

    /// What this backend measurably emits, and nothing else.
    ///
    /// Several capabilities are deliberately absent because headless `agy`
    /// provably cannot do them, each verified against agy 1.1.20 on
    /// 2026-08-25:
    ///
    /// * `ImageInput` — stream-json accepts only `"text"` content blocks and
    ///   rejects `"image"` outright.
    /// * `ReasoningDeltas` — no `thinking` or `reasoning` step is ever emitted,
    ///   even by a model billing hundreds of thinking tokens.
    /// * `TaskUpdates` and friends — `manage_task` manages background tasks,
    ///   not a plan. `agy` has no task-list tool.
    /// * `BackgroundTasks` — a backgrounded command does not outlive its turn:
    ///   `agy` holds the turn's `result` open until the task finishes, and
    ///   withholds every later step until then.
    /// * `ForkSession` — no headless fork exists.
    fn capabilities() -> tyde_agent_adapter::BackendCapabilities {
        use tyde_agent_adapter::BackendCapability as Cap;
        [
            Cap::ResumeSession,
            Cap::SetWorkspaceRoots,
            Cap::SetMultipleWorkspaceRoots,
            Cap::Interrupt,
            Cap::SessionSettings,
            Cap::StartupMcpServers,
            Cap::AgentControlTools,
            Cap::CapacityTelemetry,
            Cap::OutOfBandCapacity,
            Cap::ListSessions,
            Cap::WorkspaceInstructions,
            Cap::Customization,
            Cap::UserQuestionRequests,
            Cap::AsyncUserQuestionRequests,
            Cap::TurnUsageReported,
            Cap::ModelRequestUsageReported,
            Cap::Subagents,
            Cap::ForegroundSubagents,
            Cap::GenericModifyFile,
            Cap::GenericReadFiles,
            Cap::GenericWebSearch,
            Cap::GenericGenerateImage,
            Cap::GenericSleep,
            Cap::GenericOtherTool,
            Cap::SlashCommands,
        ]
        .into()
    }

    fn session_settings_schema() -> SessionSettingsSchema {
        SessionSettingsSchema {
            model_resolutions: Default::default(),
            backend_kind: BackendKind::Antigravity,
            fields: vec![SessionSettingField {
                key: "model".to_string(),
                label: "Model".to_string(),
                description: None,
                use_slider: false,
                select_options_by_setting: None,
                field_type: SessionSettingFieldType::Select {
                    options: Vec::new(),
                    default: None,
                    nullable: true,
                },
            }],
        }
    }

    fn has_dynamic_session_schema() -> bool {
        true
    }

    async fn discover_in_workspace(
        context: &crate::backend::BackendProbeContext,
    ) -> Result<crate::backend::BackendDiscovery, crate::backend::BackendDiscoveryError> {
        let program = context.program.as_deref().unwrap_or("agy");
        let (cwd, _) = resolve_workspace_roots(&context.workspace_roots)?;
        let options = antigravity_model_catalog(program, Path::new(&cwd)).await?;
        let default = antigravity_default_model(program, Some(Path::new(&cwd))).await?;
        if !options.iter().any(|option| option.value == default) {
            return Err(
                "Antigravity's selected model is absent from its native catalog"
                    .to_owned()
                    .into(),
            );
        }
        tracing::info!(
            model_count = options.len(),
            "Discovered Antigravity native models"
        );
        let mut schema = Self::session_settings_schema();
        schema.fields[0].field_type = SessionSettingFieldType::Select {
            options,
            default: Some(default),
            nullable: true,
        };
        Ok(crate::backend::BackendDiscovery {
            schema,
            launch_profiles: Vec::new(),
        })
    }

    fn compaction_capability(&self) -> BackendCompactionCapability {
        antigravity_compaction_capability(self.provider_version.clone())
    }

    async fn begin_compaction(&self, _request: BackendCompactionRequest) -> BackendCompactionStart {
        BackendCompactionStart::NotDispatched {
            reason: BackendCompactionNotDispatchedReason::NativeUnavailable(
                BackendCompactionUnavailableReason::ManualTriggerAbsent,
            ),
        }
    }

    async fn spawn_in_workspace(
        workspace_roots: Vec<String>,
        config: BackendSpawnConfig,
        initial_input: protocol::SendMessagePayload,
    ) -> Result<(Self, EventStream), String> {
        let conversations_dir = resolve_antigravity_conversations_dir(
            config.backend_storage.root(BackendKind::Antigravity),
        )?;
        Self::start(
            workspace_roots,
            config,
            Some(initial_input),
            None,
            &conversations_dir,
        )
        .await
    }

    async fn resume_in_workspace(
        workspace_roots: Vec<String>,
        config: BackendSpawnConfig,
        session_id: SessionId,
    ) -> Result<(Self, EventStream), String> {
        let conversations_dir = resolve_antigravity_conversations_dir(
            config.backend_storage.root(BackendKind::Antigravity),
        )?;
        ensure_antigravity_conversation_exists(&session_id, &conversations_dir)?;
        Self::start(
            workspace_roots,
            config,
            None,
            Some(session_id),
            &conversations_dir,
        )
        .await
    }

    async fn fork_in_workspace(
        _workspace_roots: Vec<String>,
        _config: BackendSpawnConfig,
        _from_session_id: SessionId,
        _initial_input: protocol::SendMessagePayload,
    ) -> Result<(Self, EventStream), BackendStartupError> {
        Err(BackendStartupError::unsupported(
            backend_fork_unsupported_message(BackendKind::Antigravity),
        ))
    }

    async fn list_sessions_in_workspace(
        _context: &crate::backend::BackendProbeContext,
    ) -> Result<Vec<BackendSession>, String> {
        let conversations_dir = resolve_antigravity_conversations_dir(None)?;
        list_antigravity_sessions(
            &conversations_dir,
            &antigravity_brain_dir(&conversations_dir),
        )
    }

    fn session_id(&self) -> SessionId {
        self.session_id.clone()
    }

    async fn update_session_settings(
        &mut self,
        payload: protocol::SetSessionSettingsPayload,
    ) -> Result<(), String> {
        let (reply, result) = oneshot::channel();
        self.settings_tx
            .send((payload.values, reply))
            .map_err(|_| "Antigravity session has closed".to_owned())?;
        result
            .await
            .map_err(|_| "Antigravity settings update channel closed".to_owned())?
    }

    fn set_workspace_roots(
        &mut self,
        workspace_roots: Vec<String>,
    ) -> impl std::future::Future<Output = Result<(), super::WorkspaceRelocationError>> + Send + 'static
    {
        let workspace_tx = self.workspace_tx.clone();
        let inner = Arc::clone(&self.inner);
        async move {
            if inner.state.with(|state| state.closing).await {
                return Err(super::WorkspaceRelocationError::Rejected(
                    "Antigravity backend is closed".to_owned(),
                ));
            }
            let roots = super::validate_local_workspace_roots(workspace_roots)
                .map_err(super::WorkspaceRelocationError::Rejected)?;
            let (reply, result) = oneshot::channel();
            workspace_tx
                .send(WorkspaceRootsRequest { roots, reply })
                .map_err(|_| {
                    super::WorkspaceRelocationError::Rejected(
                        "Antigravity session has closed".to_owned(),
                    )
                })?;
            result.await.map_err(|_| {
                super::WorkspaceRelocationError::OutcomeUnknown(
                "Antigravity closed while changing workspace roots; provider outcome is unknown"
                    .to_owned(),
            )
            })?
        }
    }

    async fn read_session_settings(&self) -> Result<SessionSettingsValues, String> {
        let model = self.inner.state.with(|state| state.model.clone()).await;
        crate::backend::session_settings_from_json(json!({"model": model}))
    }

    #[cfg(feature = "test-support")]
    async fn native_process_id_for_test(&self) -> Result<u32, String> {
        self.inner
            .state
            .with(|state| state.native_pid)
            .await
            .ok_or_else(|| "Antigravity native process is unavailable".to_owned())
    }

    async fn send(&self, input: AgentInput) -> bool {
        !self.inner.state.with(|state| state.closing).await && self.input_tx.send(input).is_ok()
    }

    async fn send_with_outcome(&self, input: AgentInput) -> crate::backend::SendOutcome {
        use crate::backend::SendOutcome;
        let (closing, active) = self
            .inner
            .state
            .with(|state| (state.closing, state.turn_active))
            .await;
        if closing {
            return SendOutcome::Closed;
        }
        // Antigravity only starts turns in response to caller input, so a
        // busy admission here means the caller dispatched against a stale
        // idle view (e.g. an actor/backend desync); hand the message back for
        // requeueing instead of letting the actor task reject and drop it.
        if let AgentInput::SendMessage(payload) = &input
            && payload.tool_response.is_none()
            && active
        {
            return SendOutcome::Busy(input);
        }
        if self.input_tx.send(input).is_ok() {
            SendOutcome::Accepted
        } else {
            SendOutcome::Closed
        }
    }

    async fn interrupt(&self) -> bool {
        self.interrupt_tx.send(()).is_ok()
    }

    /// Waits for the supervisor to confirm the `agy` process is gone.
    ///
    /// Dropping the input channel alone is not enough: the supervisor can be
    /// inside a process restart when it happens, and a returning `shutdown`
    /// that leaves the child running leaks it past the caller's teardown.
    async fn shutdown(self) {
        let done = self
            .inner
            .state
            .with(|state| {
                state.closing = true;
                state.shutdown_complete.take()
            })
            .await;
        drop(self.workspace_tx);
        drop(self.settings_tx);
        drop(self.input_tx);
        drop(self.interrupt_tx);
        if let Some(done) = done {
            let _ = done.await;
        }
    }
}

impl AntigravityBackend {
    async fn start(
        workspace_roots: Vec<String>,
        config: BackendSpawnConfig,
        initial_input: Option<protocol::SendMessagePayload>,
        resume: Option<SessionId>,
        conversations_dir: &Path,
    ) -> Result<(Self, EventStream), String> {
        if initial_input.as_ref().is_some_and(|input| {
            input
                .images
                .as_ref()
                .is_some_and(|images| !images.is_empty())
        }) {
            return Err(
                "Antigravity CLI does not support image input in headless print mode.".to_string(),
            );
        }

        let (primary_root, extra_roots) = resolve_workspace_roots(&workspace_roots)?;
        let settings = resolve_session_settings(&config);
        let model = selected_model(&settings, Some(Path::new(&primary_root))).await?;
        let combined_instructions =
            render_combined_spawn_instructions(&config.resolved_spawn_config);
        let mut skill_setup = prepare_antigravity_skills(&config.resolved_spawn_config.skills)?;
        let tool_policy_projection = prepare_antigravity_tool_policy(
            &config.resolved_spawn_config.excluded_tool_categories,
        )?;

        // Private to this session and removed with it. The descriptor holds the
        // endpoints and bearer credentials that must never reach the config
        // every `agy` process shares.
        let descriptor_dir = tempfile::Builder::new()
            .prefix("tyde-antigravity-mcp-")
            .tempdir()
            .map_err(|err| format!("Failed to create Antigravity MCP descriptor dir: {err}"))?;
        let bridge =
            install_antigravity_mcp_config(&config.startup_mcp_servers, descriptor_dir.path())
                .await?;

        // Multiple native project roots make empty-Cwd terminal calls choose
        // different folders. Keep one project cwd; expose extra roots in context.
        let workspace_roots = std::iter::once(primary_root.clone())
            .chain(extra_roots)
            .collect();
        let mut extra_roots = Vec::new();
        if let Some(projection) = skill_setup.projection.as_ref() {
            let root = projection.path().to_str().ok_or_else(|| {
                format!(
                    "Antigravity skill projection path is not valid UTF-8: {}",
                    projection.path().display()
                )
            })?;
            extra_roots.push(root.to_string());
        }
        if let Some(projection) = tool_policy_projection.as_ref() {
            extra_roots.push(
                projection
                    .path()
                    .to_str()
                    .ok_or("Antigravity tool policy path is not UTF-8")?
                    .to_owned(),
            );
        }
        let launch = AgyLaunch {
            workspace_roots,
            primary_root,
            extra_roots,
            access_mode: config.resolved_spawn_config.access_mode,
            model: model.clone(),
            bridge_env: bridge.as_ref().map(AntigravityMcpBridge::env),
        };

        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let emitter = Arc::new(TurnEmitter::new_for_agent(
            event_tx,
            AgentName(ANTIGRAVITY_AGENT_NAME),
        ));
        if let Some(notice) = skill_setup.degraded_notice.take() {
            emitter.warning_message(&notice);
        }

        let replay = match resume.as_ref() {
            Some(id) => antigravity_history(&antigravity_brain_dir(conversations_dir), &id.0)?,
            None => Vec::new(),
        };

        let resume_metadata = if let Some(id) = resume.as_ref() {
            let path = antigravity_conversation_db_path(id, conversations_dir);
            let stored = antigravity_session_workspace_roots(&path)?;
            let native_roots = launch.native_roots()?;
            if stored != native_roots {
                let previous = stored
                    .first()
                    .ok_or("Antigravity conversation has no native workspace")?;
                tracing::info!(session_id = %id.0, ?native_roots, "Restoring Antigravity primary workspace and skill roots before resume");
                let (before, after) =
                    relocate_antigravity_metadata(&path, previous, &native_roots)?;
                Some((path, before, after))
            } else {
                None
            }
        } else {
            None
        };
        // Overlaps with the process start, which itself blocks on `init`.
        let slash_commands_probe = crate::backend::subprocess::spawn(
            read_antigravity_slash_commands(launch.model.clone()),
        );
        let process = match AgyProcess::start(
            &launch,
            resume.as_ref().map(|id| id.0.as_str()),
            Arc::clone(&emitter),
        )
        .await
        {
            Ok(process) => process,
            Err(err) => {
                if let Some((path, before, after)) = resume_metadata {
                    replace_antigravity_metadata(&path, &after, &before)?;
                }
                return Err(err);
            }
        };
        let slash_commands = slash_commands_probe.await.ok().flatten();
        if let Some(catalog) = slash_commands.clone() {
            emitter.slash_commands(catalog);
        }

        let session_id = SessionId(process.conversation_id.clone());
        let inner = Arc::new(AntigravityInner {
            relocation_wake: tokio::sync::watch::channel(()).0,
            emitter: Arc::clone(&emitter),
            state: AntigravityStateActor::new(AntigravityState {
                #[cfg(feature = "test-support")]
                native_pid: process.child.id(),
                model,
                turn_active: false,
                closing: false,
                shutdown_complete: None,
            }),
        });

        // Workspace instructions ride the first prompt of a new conversation.
        // A resumed one already has them in its history, and repeating them
        // every turn would pay for them again on every request.
        let initial_turn = initial_input.map(|input| {
            let user_message = input.message;
            let provider_message = if resume.is_none() {
                build_prompt(combined_instructions.as_deref(), &user_message)
            } else {
                user_message.clone()
            };
            InitialTurn {
                provider_message,
                user_message,
                origin: input.origin,
            }
        });

        let (workspace_tx, workspace_rx) = mpsc::unbounded_channel();
        let (settings_tx, settings_rx) = mpsc::unbounded_channel();
        let (input_tx, input_rx) = mpsc::unbounded_channel::<AgentInput>();
        let (interrupt_tx, interrupt_rx) = mpsc::unbounded_channel::<()>();

        let (shutdown_complete_tx, shutdown_complete_rx) = oneshot::channel();
        inner
            .state
            .with(move |state| state.shutdown_complete = Some(shutdown_complete_rx))
            .await;
        let supervisor = Supervisor {
            inner: Arc::clone(&inner),
            launch,
            brain_dir: antigravity_brain_dir(conversations_dir),
            conversation_db: antigravity_conversation_db_path(&session_id, conversations_dir),
            descriptor_dir: Some(descriptor_dir),
            skill_projection: skill_setup.projection,
            tool_policy_projection,
            mapper: None,
            cumulative: AgyUsage::default(),
            pending_generation_usage: Some(AgyUsage::default()),
            turn_counter: 0,
            pending_questions: Vec::new(),
            deferred_answers: VecDeque::new(),
            capacity_read_at: None,
            shutdown_complete: shutdown_complete_tx,
            subagent_emitter: config.subagent_emitter.clone(),
            native_pending: BTreeMap::new(),
            native_sessions: BTreeSet::new(),
            native_children: tokio::task::JoinSet::new(),
            native_child_failure: None,
            session_id: session_id.0.clone(),
            slash_commands,
            relaunch_before_next_turn: false,
        };
        crate::backend::subprocess::spawn(async move {
            supervisor
                .run(
                    process,
                    workspace_rx,
                    settings_rx,
                    input_rx,
                    interrupt_rx,
                    initial_turn,
                )
                .await;
        });

        let (backend_tx, backend_rx) = mpsc::unbounded_channel::<BackendEvent>();
        {
            for event in replay {
                backend_tx
                    .send(BackendEvent::Chat(event))
                    .map_err(|_| "Antigravity replay receiver closed")?;
            }
        }
        if resume.is_some() {
            backend_tx
                .send(BackendEvent::ResumeReplayComplete(Ok(())))
                .map_err(|_| "Antigravity replay receiver closed")?;
        }
        crate::backend::subprocess::spawn(async move {
            let mut event_rx = event_rx;
            while let Some(raw) = event_rx.recv().await {
                let Some(event) = map_emitter_event(&raw) else {
                    continue;
                };
                if backend_tx.send(event).is_err() {
                    return;
                }
            }
        });

        Ok((
            Self {
                workspace_tx,
                settings_tx,
                input_tx,
                interrupt_tx,
                session_id,
                provider_version: config.provider_version.clone(),
                inner,
            },
            EventStream::new_backend(backend_rx),
        ))
    }
}

/// `TurnEmitter` speaks a slightly wider vocabulary than `ChatEvent`: token
/// usage rides its own `BackendEvent`, and a few kinds exist only for backends
/// that learn their session id late, which this one does not.
fn map_emitter_event(raw: &Value) -> Option<BackendEvent> {
    if let Ok(event) = serde_json::from_value::<ChatEvent>(raw.clone()) {
        return Some(BackendEvent::Chat(event));
    }
    match raw.get("kind").and_then(Value::as_str).unwrap_or_default() {
        "ModelRequestTokenUsage" => {
            serde_json::from_value::<ModelRequestTokenUsage>(raw.get("data")?.clone())
                .ok()
                .map(BackendEvent::ModelRequestTokenUsage)
        }
        "Error" => Some(BackendEvent::Chat(ChatEvent::MessageAdded(
            protocol::ChatMessage {
                origin: None,
                message_id: None,
                timestamp: now_ms(),
                sender: protocol::MessageSender::Error,
                content: raw
                    .get("data")
                    .and_then(Value::as_str)
                    .unwrap_or("Antigravity backend error")
                    .to_string(),
                reasoning: None,
                tool_calls: Vec::new(),
                model_info: None,
                token_usage: None,
                context_breakdown: None,
                images: None,
            },
        ))),
        other => {
            tracing::debug!("Antigravity emitter event {other:?} has no BackendEvent");
            None
        }
    }
}

fn antigravity_compaction_capability(
    provider_version: Option<String>,
) -> BackendCompactionCapability {
    BackendCompactionCapability {
        coordinator: BackendCompactionCoordinator::ContextOperation,
        availability: BackendCompactionAvailability::AutomaticOnly {
            reason: BackendCompactionUnavailableReason::ManualTriggerAbsent,
        },
        provider_version,
        protocol_version: None,
        evidence: BackendCompactionCapabilityEvidence::AdapterContract,
    }
}
fn resolve_workspace_roots(workspace_roots: &[String]) -> Result<(String, Vec<String>), String> {
    if workspace_roots.iter().all(|root| root.trim().is_empty()) {
        let no_root_cwd = antigravity_no_root_cwd()?;
        resolve_workspace_roots_with_no_root_cwd(workspace_roots, &no_root_cwd)
    } else {
        resolve_workspace_roots_with_no_root_cwd(workspace_roots, Path::new(""))
    }
}

fn resolve_workspace_roots_with_no_root_cwd(
    workspace_roots: &[String],
    no_root_cwd: &Path,
) -> Result<(String, Vec<String>), String> {
    let mut roots = workspace_roots
        .iter()
        .map(|root| root.trim())
        .filter(|root| !root.is_empty())
        .collect::<Vec<_>>();
    if roots.iter().any(|root| root.starts_with("ssh://")) {
        return Err("Antigravity backend requires local workspace roots".to_string());
    }
    if roots.is_empty() {
        fs::create_dir_all(no_root_cwd).map_err(|err| {
            format!(
                "Failed to create Antigravity no-root working directory {}: {err}",
                no_root_cwd.display()
            )
        })?;
        return Ok((no_root_cwd.to_string_lossy().to_string(), Vec::new()));
    }
    let primary = roots
        .first()
        .expect("empty roots returned above")
        .to_string();
    if !Path::new(&primary).is_dir() {
        return Err(format!(
            "Antigravity primary workspace root is not a directory: {primary}"
        ));
    }
    let extra = roots.drain(1..).map(str::to_string).collect::<Vec<_>>();
    Ok((primary, extra))
}

fn antigravity_no_root_cwd() -> Result<PathBuf, String> {
    Ok(crate::paths::home_dir()?
        .join(".tyde")
        .join("antigravity")
        .join("no-root"))
}
pub(crate) fn is_antigravity_native_session_id(session_id: &SessionId) -> bool {
    session_id.0.len() == 36 && Uuid::parse_str(&session_id.0).is_ok()
}

pub(crate) fn is_antigravity_session_resumable(
    session_id: &SessionId,
    conversations_dir: &Path,
) -> bool {
    is_antigravity_native_session_id(session_id)
        && antigravity_conversation_db_path(session_id, conversations_dir).is_file()
}

fn ensure_antigravity_conversation_exists(
    session_id: &SessionId,
    conversations_dir: &Path,
) -> Result<(), String> {
    let path = antigravity_conversation_db_path(session_id, conversations_dir);
    if path.is_file() {
        Ok(())
    } else {
        Err(format!(
            "Antigravity conversation {session_id} does not exist at {}; refusing to resume without an exact native agy conversation",
            path.display()
        ))
    }
}

fn antigravity_conversation_db_path(session_id: &SessionId, conversations_dir: &Path) -> PathBuf {
    conversations_dir.join(format!("{}.db", session_id.0))
}

pub(crate) fn resolve_antigravity_conversations_dir(
    configured_dir: Option<&Path>,
) -> Result<PathBuf, String> {
    match configured_dir {
        Some(path) => Ok(path.to_path_buf()),
        None => Ok(crate::paths::execution_home_dir()?
            .join(".gemini")
            .join("antigravity-cli")
            .join("conversations")),
    }
}

async fn antigravity_model_catalog(
    program: &str,
    cwd: &Path,
) -> Result<Vec<SelectOption>, BackendDiscoveryError> {
    let mut command = crate::backend::subprocess::command(program)?;
    command
        .arg("models")
        .current_dir(cwd)
        .stdin(crate::backend::subprocess::ProcessStdio::Null)
        .stdout(crate::backend::subprocess::ProcessStdio::Piped)
        .stderr(crate::backend::subprocess::ProcessStdio::Piped);
    let output = tokio::time::timeout(ANTIGRAVITY_COMMAND_TIMEOUT, command.output())
        .await
        .map_err(|_| "Antigravity model catalog timed out".to_owned())?
        .map_err(|error| format!("Cannot read Antigravity model catalog: {error}"))?;
    if !output.status.success() {
        // `agy models` has no structured status mode. Match its complete native
        // sign-in diagnostic, never a generic exit code or an error substring.
        const SIGN_IN_REQUIRED: &str = concat!(
            "Fetching available models...\n",
            "Error: Please sign in to view available models. ",
            "Launch the CLI without arguments to sign in.\n",
        );
        if output.status.code() == Some(1)
            && output.stdout.is_empty()
            && output.stderr == SIGN_IN_REQUIRED.as_bytes()
        {
            return Err(BackendDiscoveryError::authentication_required(
                "Antigravity: Please sign in to view available models. Launch the CLI without arguments to sign in."
                    .to_owned(),
            ));
        }
        return Err(format!("Antigravity model catalog failed with {}", output.status).into());
    }
    let text = String::from_utf8(output.stdout)
        .map_err(|error| format!("Invalid Antigravity model catalog encoding: {error}"))?;
    let mut models = Vec::new();
    let mut seen = BTreeSet::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let (id, label) = line
            .split_once('\t')
            .ok_or_else(|| "Antigravity model catalog row is not ID-tab-label".to_owned())?;
        if id.is_empty()
            || label.is_empty()
            || label.contains('\t')
            || !seen.insert(label.to_owned())
        {
            return Err(
                "Antigravity model catalog has an invalid or duplicate label"
                    .to_owned()
                    .into(),
            );
        }
        models.push(SelectOption {
            value: label.to_owned(),
            label: label.to_owned(),
        });
    }
    if models.is_empty() {
        return Err("Antigravity reported no selectable models"
            .to_owned()
            .into());
    }
    Ok(models)
}

async fn antigravity_default_model(program: &str, cwd: Option<&Path>) -> Result<String, String> {
    let output = run_antigravity_cli_command(program, None, cwd, "/model").await?;
    if let Some(error) = output.error {
        return Err(error);
    }
    output
        .command
        .as_ref()
        .and_then(|command| command.pointer("/data/label"))
        .and_then(Value::as_str)
        .filter(|label| !label.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| "Antigravity /model omitted the selected model label".to_owned())
}

pub(crate) fn antigravity_cost_hint_defaults(cost_hint: SpawnCostHint) -> SessionSettingsValues {
    match cost_hint {
        SpawnCostHint::Low | SpawnCostHint::Medium | SpawnCostHint::High => {
            SessionSettingsValues::default()
        }
    }
}

pub(crate) fn resolve_session_settings(config: &BackendSpawnConfig) -> SessionSettingsValues {
    resolve_backend_settings(
        config,
        &AntigravityBackend::session_settings_schema(),
        antigravity_cost_hint_defaults,
    )
}

async fn selected_model(
    values: &SessionSettingsValues,
    cwd: Option<&Path>,
) -> Result<String, String> {
    match values.0.get("model") {
        Some(SessionSettingValue::String(value)) if !value.trim().is_empty() => Ok(value.clone()),
        None | Some(SessionSettingValue::Null) => antigravity_default_model("agy", cwd).await,
        Some(_) => Err("Antigravity model setting must be a nonempty string".to_owned()),
    }
}

fn build_prompt(instructions: Option<&str>, message: &str) -> String {
    let instructions = instructions
        .map(str::trim)
        .filter(|instructions| !instructions.is_empty());
    match instructions {
        Some(instructions) => format!("{instructions}\n\n{message}"),
        None => message.to_string(),
    }
}
/// Points `agy` at this session's MCP servers without putting them in the file
/// every session shares.
///
/// `agy` reads one global `mcp_config.json`, and it re-reads it: an entry
/// removed after startup stops working mid-session (measured). So a session's
/// endpoint and its per-agent bearer credential cannot live there — every
/// concurrently running session would see every other session's server, and the
/// model picks between identically-shaped choices. Measured: a parent called
/// `tyde_af031d82_..._tyde_agent_control` while its own process had installed
/// only `5870064b-...`, so its spawn was executed by a different Tyde entirely.
///
/// Instead the shared file gets ONE static, credential-free stdio entry that
/// launches Tyde's own MCP bridge. `agy` starts that bridge once per `agy`
/// process, and the bridge inherits that process's environment, which names a
/// private descriptor holding this session's URLs and headers. One entry means
/// the model cannot pick the wrong one, and nothing secret is ever written to a
/// file other sessions can read.
async fn install_antigravity_mcp_config(
    startup_mcp_servers: &[StartupMcpServer],
    descriptor_dir: &Path,
) -> Result<Option<AntigravityMcpBridge>, String> {
    if startup_mcp_servers.is_empty() {
        return Ok(None);
    }
    let executable = crate::mcp_bridge::resolve_bridge_executable()?;
    let path = antigravity_mcp_config_path()?;
    {
        let _guard = ANTIGRAVITY_MCP_CONFIG_MUTEX.lock().await;
        let _file_lock = AntigravityMcpConfigLock::acquire(&path).await?;
        let original = read_optional_bytes(&path)?;
        if let Some(merged) = merge_antigravity_bridge_entry(original.as_deref(), &executable)
            .map_err(|err| {
                format!(
                    "Failed to prepare Antigravity MCP config {}: {err}",
                    path.display()
                )
            })?
        {
            write_bytes_atomically(&path, &merged).map_err(|err| {
                format!(
                    "Failed to write Antigravity MCP config {}: {err}",
                    path.display()
                )
            })?;
        }
    }

    let descriptor = crate::mcp_bridge::BridgeDescriptor {
        servers: startup_mcp_servers
            .iter()
            .filter_map(antigravity_bridge_server)
            .collect(),
        // `agy` issues tool calls one at a time in the runs measured here, so
        // the bridge is not asked to fan out concurrent calls.
        supports_parallel_tool_calls: false,
    };
    let descriptor_path = descriptor_dir.join(crate::mcp_bridge::DESCRIPTOR_FILE_NAME);
    let contents = serde_json::to_vec(&descriptor)
        .map_err(|err| format!("Failed to serialize Antigravity MCP descriptor: {err}"))?;
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(&descriptor_path)
        .and_then(|mut file| file.write_all(&contents))
        .map_err(|err| format!("Failed to write Antigravity MCP descriptor: {err}"))?;
    Ok(Some(AntigravityMcpBridge { descriptor_path }))
}

/// This session's private descriptor, handed to the bridge through the `agy`
/// process environment.
struct AntigravityMcpBridge {
    descriptor_path: PathBuf,
}

impl AntigravityMcpBridge {
    fn env(&self) -> (String, String) {
        (
            crate::mcp_bridge::DESCRIPTOR_ENV.to_string(),
            self.descriptor_path.to_string_lossy().to_string(),
        )
    }
}

fn antigravity_bridge_server(
    server: &StartupMcpServer,
) -> Option<crate::mcp_bridge::BridgeServerConfig> {
    let transport = match &server.transport {
        StartupMcpTransport::Http { url, headers, .. } => {
            crate::mcp_bridge::BridgeTransport::Http {
                url: url.clone(),
                headers: headers.clone(),
            }
        }
        StartupMcpTransport::Stdio { command, args, env } => {
            crate::mcp_bridge::BridgeTransport::Stdio {
                command: command.clone(),
                args: args.clone(),
                env: env.clone(),
            }
        }
    };
    Some(crate::mcp_bridge::BridgeServerConfig {
        name: server.name.clone(),
        transport,
    })
}

/// Installs the one static bridge entry, returning the bytes to write only when
/// the file actually needs to change.
///
/// Idempotent by design: the entry carries no session state, so a crashed Tyde
/// leaves nothing to clean up and the next launch finds it already correct.
fn merge_antigravity_bridge_entry(
    original_bytes: Option<&[u8]>,
    executable: &str,
) -> Result<Option<Vec<u8>>, String> {
    let mut value = match original_bytes {
        Some(bytes) if bytes.iter().all(u8::is_ascii_whitespace) => json!({}),
        Some(bytes) => serde_json::from_slice::<Value>(bytes)
            .map_err(|err| format!("existing mcp_config.json is malformed: {err}"))?,
        None => json!({}),
    };
    let object = value
        .as_object_mut()
        .ok_or_else(|| "existing mcp_config.json must be a JSON object".to_string())?;
    if !object.contains_key("mcpServers") {
        object.insert("mcpServers".to_string(), Value::Object(Map::new()));
    }
    let servers = object
        .get_mut("mcpServers")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| "existing mcp_config.json mcpServers must be a JSON object".to_string())?;

    // Entries from the previous design: one per session, each carrying a bearer
    // credential. They are the bug this replaces, so they go.
    let stale = servers
        .keys()
        .filter(|key| key.starts_with("tyde_") && key.as_str() != ANTIGRAVITY_BRIDGE_SERVER_NAME)
        .cloned()
        .collect::<Vec<_>>();
    let removed_stale = !stale.is_empty();
    for key in stale {
        servers.remove(&key);
    }

    let managed = json!({
        "command": executable,
        "args": [crate::mcp_bridge::BRIDGE_SUBCOMMAND],
    });
    let existing = servers.get(ANTIGRAVITY_BRIDGE_SERVER_NAME);
    if existing == Some(&managed) && !removed_stale {
        return Ok(None);
    }
    if let Some(existing) = existing
        && existing
            .get("args")
            .and_then(Value::as_array)
            .is_none_or(|args| {
                // Either spelling of the bridge subcommand marks the entry as
                // Tyde-managed and therefore safe to rewrite.
                !args.iter().any(|arg| {
                    arg.as_str().is_some_and(|arg| {
                        arg == crate::mcp_bridge::BRIDGE_SUBCOMMAND || arg == "mcp-bridge"
                    })
                })
            })
    {
        return Err(format!(
            "Antigravity MCP server name {ANTIGRAVITY_BRIDGE_SERVER_NAME:?} is already user-managed"
        ));
    }
    servers.insert(ANTIGRAVITY_BRIDGE_SERVER_NAME.to_string(), managed);

    serde_json::to_vec_pretty(&value)
        .map(Some)
        .map_err(|err| format!("failed to serialize merged mcp_config.json: {err}"))
}

fn antigravity_mcp_config_path() -> Result<PathBuf, String> {
    Ok(crate::paths::execution_home_dir()?
        .join(".gemini")
        .join("config")
        .join("mcp_config.json"))
}

fn read_optional_bytes(path: &Path) -> Result<Option<Vec<u8>>, String> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(format!(
            "Failed to read Antigravity MCP config {}: {err}",
            path.display()
        )),
    }
}

/// A cross-process lock over `agy`'s shared `mcp_config.json`.
///
/// The in-process mutex orders tasks inside one Tyde and nothing more. `agy`
/// reads a single user-level config, so two Tyde processes otherwise
/// read-modify-write it concurrently and lose each other's edits.
struct AntigravityMcpConfigLock {
    file: fs::File,
}

impl AntigravityMcpConfigLock {
    /// Taken on a blocking thread: `lock_exclusive` parks the calling thread,
    /// and parking a runtime worker stalls every task scheduled on it.
    async fn acquire(path: &Path) -> Result<Self, String> {
        let path = path.to_path_buf();
        crate::backend::subprocess::spawn_blocking(move || Self::acquire_blocking(&path))
            .await
            .map_err(|err| format!("Antigravity MCP config lock task failed: {err}"))?
    }

    fn acquire_blocking(path: &Path) -> Result<Self, String> {
        use fs2::FileExt;

        let lock_path = path.with_extension("json.tyde-lock");
        if let Some(parent) = lock_path.parent() {
            fs::create_dir_all(parent).map_err(|err| {
                format!(
                    "Failed to create Antigravity MCP config directory {}: {err}",
                    parent.display()
                )
            })?;
        }
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .map_err(|err| {
                format!(
                    "Failed to open Antigravity MCP config lock {}: {err}",
                    lock_path.display()
                )
            })?;
        // Reported *while* waiting, not after: a lock that is never released
        // otherwise presents as a backend that simply never started, with
        // nothing in the log to say why.
        let waited_from = std::time::Instant::now();
        let mut warned = false;
        while FileExt::try_lock_exclusive(&file).is_err() {
            if waited_from.elapsed() >= ANTIGRAVITY_MCP_LOCK_WARN_AFTER {
                tracing::warn!(
                    "Still waiting after {:.0}s for the Antigravity MCP config lock {}",
                    waited_from.elapsed().as_secs_f64(),
                    lock_path.display()
                );
                warned = true;
            }
            std::thread::sleep(ANTIGRAVITY_MCP_LOCK_POLL);
        }
        if warned {
            tracing::warn!(
                "Acquired the Antigravity MCP config lock after {:.0}s",
                waited_from.elapsed().as_secs_f64()
            );
        }
        Ok(Self { file })
    }
}

impl Drop for AntigravityMcpConfigLock {
    fn drop(&mut self) {
        use fs2::FileExt;

        let _ = FileExt::unlock(&self.file);
    }
}

fn write_bytes_atomically(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path.parent().ok_or_else(|| {
        format!(
            "Antigravity MCP config path has no parent: {}",
            path.display()
        )
    })?;
    fs::create_dir_all(parent)
        .map_err(|err| format!("failed to create MCP config directory: {err}"))?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            format!(
                "Antigravity MCP config path has no file name: {}",
                path.display()
            )
        })?;
    let tmp_path = parent.join(format!(".{file_name}.tmp.{}", now_ms()));
    let mut file = fs::File::create(&tmp_path)
        .map_err(|err| format!("failed to create temp MCP config file: {err}"))?;
    file.write_all(bytes)
        .map_err(|err| format!("failed to write temp MCP config file: {err}"))?;
    file.sync_all()
        .map_err(|err| format!("failed to sync temp MCP config file: {err}"))?;
    fs::rename(&tmp_path, path)
        .map_err(|err| format!("failed to atomically replace MCP config file: {err}"))?;
    Ok(())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}
