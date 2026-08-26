use std::collections::HashMap;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::LazyLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use command_group::{AsyncCommandGroup, AsyncGroupChild};
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
use tokio::process::{ChildStdin, Command};
use tokio::sync::{Mutex, mpsc, oneshot};
use uuid::Uuid;

use crate::backend::antigravity_stream::{
    AgyFrame, AgyResult, AgyStep, AgyUsage, AgyUsageBucket, AgyUsageReport, MCP_DISPATCH_TOOL,
    STATE_ACTIVE, STATE_DONE, STATE_ERROR, STEP_AGENT_RESPONSE, STEP_CHECKPOINT, STEP_SUBAGENT,
    STEP_SYSTEM_MESSAGE, STEP_TOOL, STEP_UNKNOWN, STEP_USER_INPUT, TranscriptReader,
    answers_from_result, exit_code_from_result, mcp_inner_call, parse_frame, subagent_request_type,
    tool_request_type, usage_report_from_frame,
};
use crate::backend::turn_emitter::{AgentName, ResponseHandle, StreamEndPayload, TurnEmitter};
use crate::backend::{
    Backend, BackendCompactionAvailability, BackendCompactionCapability,
    BackendCompactionCapabilityEvidence, BackendCompactionCoordinator,
    BackendCompactionNotDispatchedReason, BackendCompactionRequest, BackendCompactionStart,
    BackendCompactionUnavailableReason, BackendEvent, BackendSession, BackendSpawnConfig,
    BackendStartupError, EventStream, StartupMcpServer, StartupMcpTransport,
    backend_fork_unsupported_message, render_combined_spawn_instructions,
    resolve_settings as resolve_backend_settings,
};
use crate::process_env;
use crate::sub_agent::SubAgentEmitter;

const ANTIGRAVITY_AGENT_NAME: &str = "antigravity";
const ANTIGRAVITY_STARTUP_TIMEOUT: Duration = Duration::from_secs(120);
/// `/usage` answers locally in well under a second; this only bounds a probe
/// that has stopped answering.
const ANTIGRAVITY_CAPACITY_TIMEOUT: Duration = Duration::from_secs(30);
/// How often the account's quota is re-read. Refreshing on every turn would
/// spawn a process per turn to watch a number that moves in hours.
const ANTIGRAVITY_CAPACITY_REFRESH_INTERVAL: Duration = Duration::from_secs(120);
/// How long a wait for the shared MCP config lock has to reach before it is
/// worth telling someone about.
const ANTIGRAVITY_MCP_LOCK_WARN_AFTER: Duration = Duration::from_secs(5);
/// How long `agy` gets to finalize its conversation after `SIGTERM`. Measured
/// at well under a second; the margin is for a loaded machine.
const ANTIGRAVITY_GRACEFUL_EXIT_TIMEOUT: Duration = Duration::from_secs(15);
/// `agy` ends a turn on its own after this long. It is a provider-side cap on a
/// single turn, not a Tyde timeout over local state, so it stays generous.
const ANTIGRAVITY_PRINT_TIMEOUT: &str = "60m";
const ANTIGRAVITY_DEFAULT_MODEL: &str = "Gemini 3.7 Flash (Medium)";
const ANTIGRAVITY_LOW_MODEL: &str = "Gemini 3.7 Flash (Low)";
const ANTIGRAVITY_HIGH_MODEL: &str = "Gemini 3.1 Pro (High)";

static ANTIGRAVITY_MCP_CONFIG_MUTEX: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

#[derive(Clone)]
pub struct AntigravityBackend {
    input_tx: mpsc::UnboundedSender<AgentInput>,
    interrupt_tx: mpsc::UnboundedSender<()>,
    session_id: SessionId,
    provider_version: Option<String>,
    inner: Arc<AntigravityInner>,
}

struct AntigravityInner {
    emitter: Arc<TurnEmitter>,
    state: Mutex<AntigravityState>,
}

impl AntigravityBackend {
    pub(crate) async fn set_subagent_emitter(&self, emitter: Arc<dyn SubAgentEmitter>) {
        self.inner.state.lock().await.subagent_emitter = Some(emitter);
    }
}

struct AntigravityState {
    model: String,
    /// True between accepting a message and seeing that turn's `result`.
    turn_active: bool,
    closing: bool,
    /// Where a capacity report goes. Installed after spawn, so an early report
    /// has nowhere to go and is simply not taken.
    subagent_emitter: Option<Arc<dyn SubAgentEmitter>>,
    /// Resolves once the supervisor has killed the `agy` process and released
    /// its MCP config entries.
    shutdown_complete: Option<oneshot::Receiver<()>>,
}

/// Everything a spawned `agy` process needs, kept so the supervisor can restart
/// it after an interrupt without re-deriving any of it.
struct AgyLaunch {
    primary_root: String,
    extra_roots: Vec<String>,
    access_mode: BackendAccessMode,
    model: String,
    mcp_namespace: String,
    startup_mcp_servers: Vec<StartupMcpServer>,
}

impl AgyLaunch {
    fn args(&self, conversation_id: Option<&str>) -> Vec<String> {
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
        }
        args.push("--model".to_string());
        args.push(self.model.clone());
        if let Some(conversation_id) = conversation_id {
            args.push(format!("--conversation={conversation_id}"));
        }
        args.push("--add-dir".to_string());
        args.push(self.primary_root.clone());
        for root in &self.extra_roots {
            args.push("--add-dir".to_string());
            args.push(root.clone());
        }
        args
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
        let mut command = Command::new("agy");
        command.args(launch.args(resume));
        if let Some(path) = process_env::resolved_child_process_path() {
            command.env("PATH", path);
        }
        command
            .current_dir(&launch.primary_root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = command
            .group_spawn()
            .map_err(|err| format!("Failed to start Antigravity CLI: {err:?}"))?;
        let stdin = child
            .inner()
            .stdin
            .take()
            .ok_or_else(|| "Failed to capture Antigravity stdin".to_string())?;
        let stdout = child
            .inner()
            .stdout
            .take()
            .ok_or_else(|| "Failed to capture Antigravity stdout".to_string())?;
        let stderr = child
            .inner()
            .stderr
            .take()
            .ok_or_else(|| "Failed to capture Antigravity stderr".to_string())?;

        let (frames_tx, mut frames_rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
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
        tokio::spawn(async move {
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
    open_tools: HashMap<u64, OpenTool>,
    turn_usage: AgyUsage,
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
    ) -> Self {
        Self {
            turn_id: ModelTurnId(turn_id),
            model,
            response: None,
            open_tools: HashMap::new(),
            turn_usage: AgyUsage::default(),
            cumulative_usage: cumulative,
            pending_questions: Vec::new(),
            request_sequence: 0,
            transcript,
            pending_usage: None,
        }
    }

    fn handle_step(&mut self, emitter: &TurnEmitter, step: AgyStep) {
        match step.step_type.as_str() {
            STEP_AGENT_RESPONSE => self.handle_agent_response(emitter, step),
            STEP_TOOL | STEP_SUBAGENT | STEP_UNKNOWN => self.handle_tool_like(emitter, step),
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

    fn handle_tool_like(&mut self, emitter: &TurnEmitter, step: AgyStep) {
        let step_index = step.step_index;
        // An `unknown` step carries no `tool_name` and no `tool_info`, so the
        // transcript is the only place its identity exists.
        if step.step_type == STEP_UNKNOWN {
            self.transcript.refresh();
        }
        let tool_name = step
            .tool_name
            .clone()
            .or_else(|| step.tool_info.as_ref().and_then(|info| info.name.clone()))
            .or_else(|| {
                self.transcript
                    .unnamed_call_for_tool_step(step_index)
                    .map(|call| call.name.clone())
            })
            .unwrap_or_else(|| step.step_type.clone());

        match step.state.as_str() {
            STATE_ACTIVE => self.open_tool(emitter, step_index, &tool_name, &step),
            STATE_DONE | STATE_ERROR => {
                // A tool whose ACTIVE never arrived still gets a card, so the
                // work shows up rather than vanishing.
                if !self.open_tools.contains_key(&step_index) {
                    self.open_tool(emitter, step_index, &tool_name, &step);
                }
                self.close_tool(emitter, step_index, &step);
            }
            other => {
                tracing::debug!("Antigravity tool step state {other:?} has no Tyde mapping");
            }
        }
    }

    fn open_tool(
        &mut self,
        emitter: &TurnEmitter,
        step_index: u64,
        tool_name: &str,
        step: &AgyStep,
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
            self.transcript.refresh();
            let enriched = self
                .transcript
                .call_for_tool_step(step_index, tool_name)
                .map(|call| call.args.clone());
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
        emitter.tool_completed(
            &open.tool_call_id,
            ToolExecutionOutcome::Succeeded {
                result: tool_execution_result(
                    &open.tool_type,
                    &output,
                    transcript_result.as_deref(),
                ),
            },
        );
    }

    fn close_response(&mut self, emitter: &TurnEmitter) {
        let Some(open) = self.response.take() else {
            return;
        };
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
                token_usage: open.usage.map(|usage| {
                    MessageTokenUsage::request_and_turn_known(
                        usage.to_protocol(),
                        self.turn_usage.to_protocol(),
                    )
                }),
                tool_calls: open.declarations,
                ..Default::default()
            },
        );
    }

    fn take_pending_questions(&mut self) -> Vec<String> {
        std::mem::take(&mut self.pending_questions)
    }

    /// Drops the mapper's own bookkeeping for an interrupted turn.
    ///
    /// The cards themselves are closed by `TurnEmitter::operation_cancelled`,
    /// which discards the open response and completes every pending tool as
    /// cancelled. Doing it here as well would report each card twice.
    fn abandon_open_work(&mut self) {
        self.open_tools.clear();
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
    mcp_guard: Option<AntigravityMcpConfigGuard>,
    mapper: Option<TurnMapper>,
    cumulative: AgyUsage,
    turn_counter: u64,
    /// Question cards waiting on a human. They survive the turn that asked, and
    /// are cleared either by an answer or by a cancel.
    pending_questions: Vec<String>,
    /// When the account's quota was last read, so a busy session does not spawn
    /// a probe per turn.
    capacity_read_at: Option<std::time::Instant>,
    shutdown_complete: oneshot::Sender<()>,
}

impl Supervisor {
    async fn run(
        mut self,
        mut process: AgyProcess,
        mut input_rx: mpsc::UnboundedReceiver<AgentInput>,
        mut interrupt_rx: mpsc::UnboundedReceiver<()>,
        initial_message: Option<String>,
    ) {
        let emitter = Arc::clone(&self.inner.emitter);

        if let Some(message) = initial_message
            && !self.start_turn(&mut process, &message, true).await
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
                }

                frame = process.frames_rx.recv() => {
                    match frame {
                        Some(Ok(frame)) => self.handle_frame(frame).await,
                        // A line we cannot read is the provider telling us
                        // something we do not understand. Surfacing it beats
                        // dropping it and reporting a turn that quietly lost
                        // half its events.
                        Some(Err(err)) => emitter.backend_error(&err),
                        None => {
                            if self.inner.state.lock().await.turn_active {
                                emitter.backend_error(
                                    "Antigravity CLI exited while a turn was still running",
                                );
                                self.finish_turn().await;
                            }
                            break;
                        }
                    }
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
        // `operation_cancelled` completes every pending tool as cancelled,
        // including any question still waiting on the user.
        self.pending_questions.clear();
        self.inner
            .emitter
            .operation_cancelled("Antigravity turn cancelled.");
        self.inner.state.lock().await.turn_active = false;

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

    async fn handle_input(&mut self, process: &mut AgyProcess, input: AgentInput) -> bool {
        match input {
            AgentInput::SendMessage(payload) => {
                if let Some(response) = payload.tool_response.clone()
                    && !self.answer_question(response).await
                {
                    return true;
                }
                // A tool response is not a chat message, so it produces no user
                // bubble — the card the user acted on is the record of it.
                let echo = payload.tool_response.is_none();
                self.start_turn(process, &payload.message, echo).await
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
            AgentInput::EditQueuedMessage(_)
            | AgentInput::CancelQueuedMessage(_)
            | AgentInput::SendQueuedMessageNow(_) => {
                panic!(
                    "queued-message inputs must be handled by the agent actor before reaching the \
                     backend"
                );
            }
        }
    }

    /// Closes the card the user acted on, so their answer and the transcript
    /// agree, and reports a mismatch rather than silently dropping it.
    async fn answer_question(&mut self, response: protocol::SendMessageToolResponse) -> bool {
        let protocol::SendMessageToolResponse::AskUserQuestion {
            tool_call_id,
            answer,
        } = response
        else {
            self.inner.emitter.backend_error(
                "Antigravity received a plan-approval response, which it never requests",
            );
            return false;
        };
        let Some(position) = self
            .pending_questions
            .iter()
            .position(|pending| *pending == tool_call_id)
        else {
            self.inner.emitter.backend_error(&format!(
                "Antigravity received an answer for question {tool_call_id}, which is not waiting \
                 on one"
            ));
            return false;
        };
        self.pending_questions.remove(position);
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
        echo_user_message: bool,
    ) -> bool {
        self.turn_counter += 1;
        let model = {
            let mut state = self.inner.state.lock().await;
            state.turn_active = true;
            state.model.clone()
        };
        self.mapper = Some(TurnMapper::new(
            format!("{}-{}", process.conversation_id, self.turn_counter),
            model,
            TranscriptReader::new(&self.brain_dir, &process.conversation_id),
            self.cumulative,
        ));
        if echo_user_message {
            self.inner.emitter.user_message(message, None);
        }
        self.inner.emitter.typing_status_changed(true);
        if let Err(err) = process.send_turn(message).await {
            self.inner.emitter.backend_error(&err);
            self.inner.emitter.typing_status_changed(false);
            self.inner.state.lock().await.turn_active = false;
            self.mapper = None;
            return false;
        }
        true
    }

    async fn handle_frame(&mut self, frame: AgyFrame) {
        match frame {
            AgyFrame::Step(step) => {
                if let Some(mapper) = self.mapper.as_mut() {
                    mapper.handle_step(&self.inner.emitter, step);
                }
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

    async fn finish_turn(&mut self) {
        if let Some(mut mapper) = self.mapper.take() {
            mapper.close_response(&self.inner.emitter);
            self.pending_questions
                .extend(mapper.take_pending_questions());
        }
        self.inner.emitter.typing_status_changed(false);
        self.inner.state.lock().await.turn_active = false;
        self.refresh_capacity().await;
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
        let (emitter, model) = {
            let state = self.inner.state.lock().await;
            (state.subagent_emitter.clone(), state.model.clone())
        };
        let Some(emitter) = emitter else {
            return;
        };
        self.capacity_read_at = Some(std::time::Instant::now());
        tokio::spawn(async move {
            let state = match read_antigravity_capacity(&model).await {
                Ok(report) => protocol::BackendCapacityState::Known { report },
                Err(reason) => protocol::BackendCapacityState::Unavailable { reason },
            };
            emitter.on_backend_capacity(BackendKind::Antigravity, state);
        });
    }

    /// A model change is a process restart: `--model` is a launch flag and
    /// `agy` takes no equivalent over stdin. The conversation survives it.
    async fn apply_settings(
        &mut self,
        process: &mut AgyProcess,
        values: SessionSettingsValues,
    ) -> Result<(), String> {
        let model = selected_model(&values)?;
        {
            let mut state = self.inner.state.lock().await;
            if state.model == model {
                return Ok(());
            }
            state.model = model.clone();
        }
        self.launch.model = model;
        let conversation_id = process.conversation_id.clone();
        let replacement = AgyProcess::start(
            &self.launch,
            Some(&conversation_id),
            Arc::clone(&self.inner.emitter),
        )
        .await?;
        process.terminate().await;
        *process = replacement;
        Ok(())
    }

    async fn shutdown(self, mut process: AgyProcess) {
        process.terminate().await;
        if let Some(guard) = self.mcp_guard
            && let Err(err) = guard.remove(&self.launch.startup_mcp_servers).await
        {
            tracing::warn!("{err}");
        }
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
) -> ToolExecutionResult {
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
            // `agy` summarises a read as "4 lines, 17 bytes" and reports the
            // size nowhere else, so the size is measured rather than parsed out
            // of a sentence whose wording is not a contract.
            files: file_paths
                .iter()
                .map(|path| protocol::FileInfo {
                    path: path.clone(),
                    bytes: fs::metadata(path).map(|meta| meta.len()).unwrap_or(0),
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

/// Reads the account's remaining quota without starting a turn.
///
/// `/usage` is answered by print mode directly — no model request, no turn,
/// zero tokens — so this costs a short-lived process and nothing else.
async fn read_antigravity_capacity(
    model: &str,
) -> Result<CapacityReport, CapacityUnavailableReason> {
    let mut command = Command::new("agy");
    command.args([
        "--output-format",
        "stream-json",
        "--model",
        model,
        "-p",
        "/usage",
    ]);
    if let Some(path) = process_env::resolved_child_process_path() {
        command.env("PATH", path);
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());

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
fn list_antigravity_sessions(conversations_dir: &Path, brain_dir: &Path) -> Vec<BackendSession> {
    let Ok(entries) = fs::read_dir(conversations_dir) else {
        return Vec::new();
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
            Some(BackendSession {
                id: session_id,
                backend_kind: BackendKind::Antigravity,
                // Not recoverable. The store records no workspace, and the
                // transcript mentions directories only inside tool arguments,
                // which is where a command ran rather than where the
                // conversation was rooted. Resume takes its roots from the
                // caller, so this is display-only.
                workspace_roots: Vec::new(),
                title: opening.as_ref().and_then(|opening| opening.title.clone()),
                token_count: None,
                created_at_ms: opening.as_ref().and_then(|opening| opening.created_at_ms),
                updated_at_ms,
                resumable: true,
            })
        })
        .collect::<Vec<_>>();
    // Most recently touched first, which is the order a resume picker wants.
    sessions.sort_by_key(|session| std::cmp::Reverse(session.updated_at_ms));
    sessions
}

struct AntigravityTranscriptOpening {
    title: Option<String>,
    created_at_ms: Option<u64>,
}

/// The first step of a conversation, which is what names it.
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
            Cap::Interrupt,
            Cap::SessionSettings,
            Cap::StartupMcpServers,
            Cap::AgentControlTools,
            Cap::CapacityTelemetry,
            Cap::ListSessions,
            Cap::WorkspaceInstructions,
            Cap::Customization,
            Cap::UserQuestionRequests,
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
        ]
        .into()
    }

    fn session_settings_schema() -> SessionSettingsSchema {
        SessionSettingsSchema {
            backend_kind: BackendKind::Antigravity,
            fields: vec![SessionSettingField {
                key: "model".to_string(),
                label: "Model".to_string(),
                description: None,
                use_slider: false,
                select_options_by_setting: None,
                field_type: SessionSettingFieldType::Select {
                    options: antigravity_known_models(),
                    default: Some(ANTIGRAVITY_DEFAULT_MODEL.to_string()),
                    nullable: false,
                },
            }],
        }
    }

    fn compaction_capability(&self) -> BackendCompactionCapability {
        antigravity_compaction_capability(self.provider_version.clone())
    }

    async fn begin_compaction(&self, _request: BackendCompactionRequest) -> BackendCompactionStart {
        BackendCompactionStart::NotDispatched {
            reason: BackendCompactionNotDispatchedReason::NativeUnavailable(
                BackendCompactionUnavailableReason::ManualTriggerAbsent,
            ),
            fallback_safe: true,
        }
    }

    async fn spawn(
        workspace_roots: Vec<String>,
        config: BackendSpawnConfig,
        initial_input: protocol::SendMessagePayload,
    ) -> Result<(Self, EventStream), String> {
        let conversations_dir =
            resolve_antigravity_conversations_dir(config.antigravity_conversations_dir.as_deref())?;
        Self::spawn_with_conversations_dir(
            workspace_roots,
            config,
            initial_input,
            conversations_dir,
        )
        .await
    }

    async fn resume(
        workspace_roots: Vec<String>,
        config: BackendSpawnConfig,
        session_id: SessionId,
    ) -> Result<(Self, EventStream), String> {
        let conversations_dir =
            resolve_antigravity_conversations_dir(config.antigravity_conversations_dir.as_deref())?;
        Self::resume_with_conversations_dir(workspace_roots, config, session_id, conversations_dir)
            .await
    }

    async fn fork(
        _workspace_roots: Vec<String>,
        _config: BackendSpawnConfig,
        _from_session_id: SessionId,
        _initial_input: protocol::SendMessagePayload,
    ) -> Result<(Self, EventStream), BackendStartupError> {
        Err(BackendStartupError::unsupported(
            backend_fork_unsupported_message(BackendKind::Antigravity),
        ))
    }

    async fn list_sessions() -> Result<Vec<BackendSession>, String> {
        let conversations_dir = resolve_antigravity_conversations_dir(None)?;
        Ok(list_antigravity_sessions(
            &conversations_dir,
            &antigravity_brain_dir(&conversations_dir),
        ))
    }

    fn session_id(&self) -> SessionId {
        self.session_id.clone()
    }

    async fn send(&self, input: AgentInput) -> bool {
        self.input_tx.send(input).is_ok()
    }

    async fn send_with_outcome(&self, input: AgentInput) -> crate::backend::SendOutcome {
        use crate::backend::SendOutcome;
        // Antigravity only starts turns in response to caller input, so a
        // busy admission here means the caller dispatched against a stale
        // idle view (e.g. an actor/backend desync); hand the message back for
        // requeueing instead of letting the actor task reject and drop it.
        if let AgentInput::SendMessage(payload) = &input
            && payload.tool_response.is_none()
            && self.inner.state.lock().await.turn_active
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
        let done = {
            let mut state = self.inner.state.lock().await;
            state.closing = true;
            state.shutdown_complete.take()
        };
        drop(self.input_tx);
        drop(self.interrupt_tx);
        if let Some(done) = done {
            let _ = done.await;
        }
    }
}

impl AntigravityBackend {
    pub(crate) async fn spawn_with_conversations_dir(
        workspace_roots: Vec<String>,
        config: BackendSpawnConfig,
        initial_input: protocol::SendMessagePayload,
        conversations_dir: PathBuf,
    ) -> Result<(Self, EventStream), String> {
        Self::start(
            workspace_roots,
            config,
            Some(initial_input),
            None,
            &conversations_dir,
        )
        .await
    }

    pub(crate) async fn resume_with_conversations_dir(
        workspace_roots: Vec<String>,
        config: BackendSpawnConfig,
        session_id: SessionId,
        conversations_dir: PathBuf,
    ) -> Result<(Self, EventStream), String> {
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
        let model = selected_model(&settings)?;
        let combined_instructions =
            render_combined_spawn_instructions(&config.resolved_spawn_config);

        // Namespaced by conversation so two Antigravity sessions can hold
        // entries in the shared config at once. A resumed session reuses its
        // conversation's namespace; a fresh one cannot know its id yet, so it
        // takes a random one.
        let mcp_namespace = resume
            .as_ref()
            .map(|id| id.0.clone())
            .unwrap_or_else(|| Uuid::new_v4().to_string());

        let launch = AgyLaunch {
            primary_root,
            extra_roots,
            access_mode: config.resolved_spawn_config.access_mode,
            model: model.clone(),
            mcp_namespace,
            startup_mcp_servers: config.startup_mcp_servers.clone(),
        };

        let mcp_guard =
            install_antigravity_mcp_config(&launch.mcp_namespace, &launch.startup_mcp_servers)
                .await?;

        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let emitter = Arc::new(TurnEmitter::new_for_agent(
            event_tx,
            AgentName(ANTIGRAVITY_AGENT_NAME),
        ));

        let process = match AgyProcess::start(
            &launch,
            resume.as_ref().map(|id| id.0.as_str()),
            Arc::clone(&emitter),
        )
        .await
        {
            Ok(process) => process,
            Err(err) => {
                if let Some(guard) = mcp_guard {
                    let _ = guard.remove(&launch.startup_mcp_servers).await;
                }
                return Err(err);
            }
        };

        let session_id = SessionId(process.conversation_id.clone());
        let inner = Arc::new(AntigravityInner {
            emitter: Arc::clone(&emitter),
            state: Mutex::new(AntigravityState {
                model,
                turn_active: false,
                closing: false,
                subagent_emitter: None,
                shutdown_complete: None,
            }),
        });

        // Workspace instructions ride the first prompt of a new conversation.
        // A resumed one already has them in its history, and repeating them
        // every turn would pay for them again on every request.
        let initial_message = initial_input.map(|input| {
            if resume.is_none() {
                build_prompt(combined_instructions.as_deref(), &input.message)
            } else {
                input.message
            }
        });

        let (input_tx, input_rx) = mpsc::unbounded_channel::<AgentInput>();
        let (interrupt_tx, interrupt_rx) = mpsc::unbounded_channel::<()>();

        let (shutdown_complete_tx, shutdown_complete_rx) = oneshot::channel();
        inner.state.lock().await.shutdown_complete = Some(shutdown_complete_rx);
        let supervisor = Supervisor {
            inner: Arc::clone(&inner),
            launch,
            brain_dir: antigravity_brain_dir(conversations_dir),
            mcp_guard,
            mapper: None,
            cumulative: AgyUsage::default(),
            turn_counter: 0,
            pending_questions: Vec::new(),
            capacity_read_at: None,
            shutdown_complete: shutdown_complete_tx,
        };
        tokio::spawn(async move {
            supervisor
                .run(process, input_rx, interrupt_rx, initial_message)
                .await;
        });

        let (backend_tx, backend_rx) = mpsc::unbounded_channel::<BackendEvent>();
        tokio::spawn(async move {
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
        None => Ok(crate::paths::home_dir()?
            .join(".gemini")
            .join("antigravity-cli")
            .join("conversations")),
    }
}

pub(crate) fn antigravity_known_models() -> Vec<SelectOption> {
    // `--model` takes the display label, not the id, and these are the labels
    // `agy models` reports for agy 1.1.20.
    [
        ANTIGRAVITY_LOW_MODEL,
        ANTIGRAVITY_DEFAULT_MODEL,
        "Gemini 3.7 Flash (High)",
        "Gemini 3.6 Flash (Low)",
        "Gemini 3.6 Flash (Medium)",
        "Gemini 3.6 Flash (High)",
        "Gemini 3.1 Pro (Low)",
        ANTIGRAVITY_HIGH_MODEL,
        "Claude Sonnet 4.6 (Thinking)",
        "Claude Opus 4.6 (Thinking)",
        "GPT-OSS 120B (Medium)",
    ]
    .into_iter()
    .map(|label| SelectOption {
        value: label.to_string(),
        label: label.to_string(),
    })
    .collect()
}

pub(crate) fn antigravity_cost_hint_defaults(cost_hint: SpawnCostHint) -> SessionSettingsValues {
    let model = match cost_hint {
        SpawnCostHint::Low => ANTIGRAVITY_LOW_MODEL,
        SpawnCostHint::Medium => ANTIGRAVITY_DEFAULT_MODEL,
        SpawnCostHint::High => ANTIGRAVITY_HIGH_MODEL,
    };
    let mut values = SessionSettingsValues::default();
    values.0.insert(
        "model".to_string(),
        SessionSettingValue::String(model.to_string()),
    );
    values
}

pub(crate) fn resolve_session_settings(config: &BackendSpawnConfig) -> SessionSettingsValues {
    resolve_backend_settings(
        config,
        &AntigravityBackend::session_settings_schema(),
        antigravity_cost_hint_defaults,
    )
}

fn selected_model(values: &SessionSettingsValues) -> Result<String, String> {
    match values.0.get("model") {
        Some(SessionSettingValue::String(value)) if is_known_model(value) => Ok(value.clone()),
        Some(SessionSettingValue::String(value)) => Err(format!(
            "unknown Antigravity model label {value:?}; expected one of the known agy model labels"
        )),
        Some(other) => Err(format!(
            "Antigravity model setting must be a string, got {other:?}"
        )),
        None => Ok(ANTIGRAVITY_DEFAULT_MODEL.to_string()),
    }
}

fn is_known_model(value: &str) -> bool {
    antigravity_known_models()
        .into_iter()
        .any(|option| option.value == value)
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
/// Adds this session's MCP servers to `agy`'s shared config.
///
/// `agy` reads `mcp_config.json` once, at process start, and takes no
/// per-invocation override, so the entries have to be in the file for the whole
/// session rather than for the length of one turn.
///
/// The previous implementation snapshotted the file and restored those exact
/// bytes afterwards. With a session-long hold that is actively wrong: two
/// Antigravity sessions overlap, and whichever finishes second restores a
/// snapshot that erases the other's servers. Entries are namespaced per
/// conversation already, so shutdown removes this session's keys and leaves
/// every other key alone.
async fn install_antigravity_mcp_config(
    namespace: &str,
    startup_mcp_servers: &[StartupMcpServer],
) -> Result<Option<AntigravityMcpConfigGuard>, String> {
    if startup_mcp_servers.is_empty() {
        return Ok(None);
    }
    let path = antigravity_mcp_config_path()?;
    let _guard = ANTIGRAVITY_MCP_CONFIG_MUTEX.lock().await;
    let _file_lock = AntigravityMcpConfigLock::acquire(&path).await?;
    let original = read_optional_bytes(&path)?;
    let merged = merge_antigravity_mcp_config(original.as_deref(), namespace, startup_mcp_servers)
        .map_err(|err| {
            format!(
                "Failed to prepare Antigravity MCP config {}: {err}",
                path.display()
            )
        })?;
    write_bytes_atomically(&path, &merged).map_err(|err| {
        format!(
            "Failed to write Antigravity MCP config {}: {err}",
            path.display()
        )
    })?;
    Ok(Some(AntigravityMcpConfigGuard {
        path,
        namespace: namespace.to_string(),
    }))
}

struct AntigravityMcpConfigGuard {
    path: PathBuf,
    namespace: String,
}

impl AntigravityMcpConfigGuard {
    async fn remove(self, startup_mcp_servers: &[StartupMcpServer]) -> Result<(), String> {
        let _guard = ANTIGRAVITY_MCP_CONFIG_MUTEX.lock().await;
        let _file_lock = AntigravityMcpConfigLock::acquire(&self.path).await?;
        let Some(bytes) = read_optional_bytes(&self.path)? else {
            return Ok(());
        };
        let mut value = serde_json::from_slice::<Value>(&bytes).map_err(|err| {
            format!(
                "Failed to read Antigravity MCP config {} for cleanup: {err}",
                self.path.display()
            )
        })?;
        let Some(servers) = value
            .as_object_mut()
            .and_then(|object| object.get_mut("mcpServers"))
            .and_then(Value::as_object_mut)
        else {
            return Ok(());
        };
        for server in startup_mcp_servers {
            servers.remove(&antigravity_mcp_server_key(&self.namespace, &server.name));
        }
        let serialized = serde_json::to_vec_pretty(&value).map_err(|err| {
            format!("Failed to serialize Antigravity MCP config for cleanup: {err}")
        })?;
        write_bytes_atomically(&self.path, &serialized).map_err(|err| {
            format!(
                "Failed to write Antigravity MCP config {}: {err}",
                self.path.display()
            )
        })
    }
}

/// A cross-process lock over `agy`'s shared `mcp_config.json`.
///
/// The in-process mutex orders tasks inside one Tyde, and that is all it can
/// do. `agy` reads a single user-level config, so two Tyde processes — or the
/// conformance suite, where every scenario is its own process — otherwise
/// read-modify-write the same file concurrently and drop each other's servers.
/// A backend that quietly loses its MCP entries looks exactly like a model that
/// declined to use a tool, which is the worst way for this to fail.
struct AntigravityMcpConfigLock {
    file: fs::File,
}

impl AntigravityMcpConfigLock {
    /// Taken on a blocking thread: `lock_exclusive` parks the calling thread
    /// until the holder releases, and parking a runtime worker stalls every
    /// other task scheduled on it — including, when several Tyde processes
    /// contend, the child agent whose own spawn is waiting behind it.
    async fn acquire(path: &Path) -> Result<Self, String> {
        let path = path.to_path_buf();
        tokio::task::spawn_blocking(move || Self::acquire_blocking(&path))
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
        // Reported rather than waited on silently. This is the only blocking
        // step between deciding to start a backend and actually starting it,
        // so a lock that is never released presents as an agent that was
        // created and then simply never ran — with nothing in the log to say
        // why.
        let waited_from = std::time::Instant::now();
        if FileExt::try_lock_exclusive(&file).is_err() {
            FileExt::lock_exclusive(&file).map_err(|err| {
                format!(
                    "Failed to lock Antigravity MCP config {}: {err}",
                    lock_path.display()
                )
            })?;
            let waited = waited_from.elapsed();
            if waited >= ANTIGRAVITY_MCP_LOCK_WARN_AFTER {
                tracing::warn!(
                    "Waited {:.1}s for the Antigravity MCP config lock {}",
                    waited.as_secs_f64(),
                    lock_path.display()
                );
            }
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
fn antigravity_mcp_config_path() -> Result<PathBuf, String> {
    Ok(crate::paths::home_dir()?
        .join(".gemini")
        .join("config")
        .join("mcp_config.json"))
}
fn merge_antigravity_mcp_config(
    original_bytes: Option<&[u8]>,
    namespace: &str,
    startup_mcp_servers: &[StartupMcpServer],
) -> Result<Vec<u8>, String> {
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

    let pruned = prune_dead_tyde_mcp_servers(servers);
    if pruned > 0 {
        tracing::info!("Removed {pruned} Antigravity MCP entries left by dead Tyde sessions");
    }

    for server in startup_mcp_servers {
        let Some(config) = antigravity_mcp_server_config(server) else {
            continue;
        };
        let key = antigravity_mcp_server_key(namespace, &server.name);
        if servers.contains_key(&key) {
            return Err(format!(
                "Tyde MCP server key {key:?} already exists in Antigravity MCP config"
            ));
        }
        servers.insert(key, config);
    }

    serde_json::to_vec_pretty(&value)
        .map_err(|err| format!("failed to serialize merged mcp_config.json: {err}"))
}

/// Drops the entries other Tyde sessions left behind when they died.
///
/// `mcp_config.json` outlives every process that writes to it, and Tyde's
/// servers are loopback HTTP endpoints on a port that belongs to one run. A
/// session that is killed rather than shut down never removes its own keys, so
/// they accumulate: measured after a day of conformance runs, 32 entries, all
/// pointing at ports nothing was listening on. That is not merely untidy —
/// `agy` advertises every configured server, so a model told to use
/// `tyde_spawn_agent` picks between sixteen of them and mostly reaches a dead
/// one, which fails the call with `connection refused`.
///
/// Liveness is the test rather than age or ownership: an entry whose port
/// answers belongs to a Tyde that is still running and is left alone, and one
/// that refuses is garbage by construction. Only `tyde_`-prefixed keys are
/// considered, so a user's own servers are never touched.
fn prune_dead_tyde_mcp_servers(servers: &mut Map<String, Value>) -> usize {
    let dead = servers
        .iter()
        .filter(|(key, value)| {
            key.starts_with("tyde_")
                && tyde_mcp_loopback_port(value).is_some_and(|port| !loopback_port_is_open(port))
        })
        .map(|(key, _)| key.clone())
        .collect::<Vec<_>>();
    for key in &dead {
        servers.remove(key);
    }
    dead.len()
}

fn tyde_mcp_loopback_port(value: &Value) -> Option<u16> {
    let url = value
        .get("serverUrl")
        .or_else(|| value.get("url"))
        .and_then(Value::as_str)?;
    let rest = url
        .strip_prefix("http://127.0.0.1:")
        .or_else(|| url.strip_prefix("http://localhost:"))?;
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    rest[..end].parse::<u16>().ok()
}

/// A refused loopback connect returns immediately, so this costs a syscall per
/// entry rather than a wait.
fn loopback_port_is_open(port: u16) -> bool {
    use std::net::{Ipv4Addr, SocketAddr, TcpStream};

    TcpStream::connect_timeout(
        &SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
        Duration::from_millis(200),
    )
    .is_ok()
}

fn antigravity_mcp_server_key(namespace: &str, server_name: &str) -> String {
    format!(
        "tyde_{}_{}",
        sanitize_mcp_key_component(namespace),
        sanitize_mcp_key_component(server_name)
    )
}

fn sanitize_mcp_key_component(value: &str) -> String {
    let sanitized = value
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' })
        .collect::<String>();
    let trimmed = sanitized.trim_matches('_');
    if trimmed.is_empty() {
        "unnamed".to_string()
    } else {
        trimmed.to_string()
    }
}

fn antigravity_mcp_server_config(server: &StartupMcpServer) -> Option<Value> {
    let name = server.name.trim();
    if name.is_empty() {
        return None;
    }
    match &server.transport {
        StartupMcpTransport::Stdio { command, args, env } => {
            build_stdio_mcp_config(command, args, env)
        }
        StartupMcpTransport::Http { url, headers, .. } => build_http_mcp_config(url, headers),
    }
}

fn build_stdio_mcp_config(
    command: &str,
    args: &[String],
    env: &HashMap<String, String>,
) -> Option<Value> {
    let command = command.trim();
    if command.is_empty() {
        return None;
    }
    let mut cfg = Map::new();
    cfg.insert("command".to_string(), Value::String(command.to_string()));
    cfg.insert(
        "args".to_string(),
        to_value(args).expect("Vec<String> is always serializable"),
    );
    if !env.is_empty() {
        cfg.insert(
            "env".to_string(),
            to_value(env).expect("HashMap<String, String> is always serializable"),
        );
    }
    Some(Value::Object(cfg))
}

fn build_http_mcp_config(url: &str, headers: &HashMap<String, String>) -> Option<Value> {
    let url = url.trim();
    if url.is_empty() {
        return None;
    }
    let mut cfg = Map::new();
    cfg.insert("serverUrl".to_string(), Value::String(url.to_string()));
    if !headers.is_empty() {
        cfg.insert(
            "headers".to_string(),
            to_value(headers).expect("HashMap<String, String> is always serializable"),
        );
    }
    Some(Value::Object(cfg))
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
