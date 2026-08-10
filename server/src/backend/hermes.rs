use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{fs, io};

use command_group::{AsyncCommandGroup, AsyncGroupChild};
use protocol::{
    AgentInput, BackendConfigSnapshotStatus, BackendKind, BackendNativeSettingsSnapshot,
    BackendSetupDiagnosticCode, BackgroundTaskState, BackgroundTaskStatus, ChatEvent, ChatMessage,
    CompactionMethod, CompactionMetrics, CompactionStage, ContextBreakdown, MessageSender,
    MessageTokenUsage, ModelInfo, OperationCancelledData, RetryAttemptData, SelectOption,
    SendMessageToolResponse, SessionId, SessionSettingField, SessionSettingFieldType,
    SessionSettingValue, SessionSettingsSchema, SessionSettingsValues, StreamEndData,
    StreamStartData, StreamTextDeltaData, TokenUsage, TokenUsageScope, TokenUsageUnavailableReason,
    ToolExecutionCompletedData, ToolExecutionResult, ToolProgressData, ToolProgressUpdate,
    ToolRequest, ToolRequestType, ToolUseData,
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStderr, ChildStdout, Command};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use crate::agent::customization::{ResolvedSpawnConfig, SkillSelection};
use crate::backend::agent_control_progress::{
    PendingToolNormalizationFailure, await_progress_data_for_tool, normalize_tyde_chat_event,
    spawn_progress_data_for_tool_result,
};
use crate::backend::hermes_config::{self, HermesProfileRef};
use crate::backend::{
    Backend, BackendAcceptedCompaction, BackendCompactionCapability,
    BackendCompactionCapabilityEvidence, BackendCompactionDeferredReason,
    BackendCompactionDispatchState, BackendCompactionEvent, BackendCompactionFailure,
    BackendCompactionFailureKind, BackendCompactionMechanism, BackendCompactionMutationState,
    BackendCompactionNotDispatchedReason, BackendCompactionProgress, BackendCompactionRequest,
    BackendCompactionResult, BackendCompactionStart, BackendCompactionSuccess,
    BackendCompactionTerminalEvidence, BackendCompactionUnavailableReason, BackendEvent,
    BackendSession, BackendSpawnConfig, BackendStartupError, EventStream, PostCompactionTokenCount,
    StartupMcpServer, StartupMcpTransport, backend_fork_unsupported_message,
    render_combined_spawn_instructions, resolve_settings as resolve_backend_settings,
    tyde_owned_no_root_cwd,
};
use crate::hermes_mcp_bridge::{
    BridgeDescriptor, BridgeServerConfig, BridgeTransport, DESCRIPTOR_ENV, DESCRIPTOR_FILE_NAME,
    MANAGED_SERVER_NAME, READY_FILE_NAME,
};
use crate::process_env;
use crate::sub_agent::{SubAgentEmitter, SubAgentHandle};

const HERMES_AGENT_NAME: &str = "hermes";
const HERMES_PYTHON_MODULE: &str = "tui_gateway.entry";
const HERMES_EXECUTABLE_ENV: &str = "HERMES_EXECUTABLE";
const HERMES_CLI_BINARY: &str = "hermes";
const HERMES_STARTUP_TIMEOUT_ENV: &str = "HERMES_TUI_STARTUP_TIMEOUT_MS";
const HERMES_RPC_TIMEOUT_ENV: &str = "HERMES_TUI_RPC_TIMEOUT_MS";
const HERMES_REMOTE_PYTHON_ENV: &str = "TYDE_REMOTE_HERMES_PYTHON";
const HERMES_BRIDGE_EXECUTABLE_ENV: &str = "TYDE_HERMES_BRIDGE_EXECUTABLE";
const HERMES_STARTUP_TIMEOUT: Duration = Duration::from_secs(15);
const HERMES_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const HERMES_USAGE_TIMEOUT: Duration = Duration::from_secs(2);
const HERMES_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(not(test))]
const HERMES_SHUTDOWN_GRACE: Duration = Duration::from_secs(4);
#[cfg(test)]
const HERMES_SHUTDOWN_GRACE: Duration = Duration::from_millis(100);
const HERMES_MODEL_PROVIDER_FLAG: &str = " --provider ";
const HERMES_TOOLSETS_ENV: &str = "HERMES_TUI_TOOLSETS";
const HERMES_TOOL_PROGRESS_ENV: &str = "HERMES_TUI_TOOL_PROGRESS";
const HERMES_MANAGED_DIR_ENV: &str = "HERMES_MANAGED_DIR";
const TYDE_HERMES_SYSTEM_PROMPT_ENV: &str = "TYDE_HERMES_SYSTEM_PROMPT";
const HERMES_MANAGED_MCP_TOOLSET: &str = "mcp-tyde";
/// Hermes's Tool Search bridge tool. When deferral is active the
/// model-visible tool list carries this (plus tool_describe/tool_call)
/// instead of the deferred MCP tools; its presence marks a session that
/// reaches MCP tools through the bridge.
const HERMES_TOOL_SEARCH_BRIDGE_TOOL: &str = "tool_search";
const HERMES_MCP_GATEWAY_ENTRY: &str = r#"
import os
import sys
import threading

from hermes_cli.env_loader import load_hermes_dotenv

load_hermes_dotenv()
from tui_gateway import server as _tyde_gateway_server

_tyde_original_emit = _tyde_gateway_server._emit
_tyde_original_get_usage = _tyde_gateway_server._get_usage
_tyde_original_make_agent = _tyde_gateway_server._make_agent
_tyde_tool_start_args = {}
_tyde_message_condition = threading.Condition()
_tyde_open_messages = set()

def _tyde_get_usage(agent):
    usage = dict(_tyde_original_get_usage(agent))
    if agent is not None:
        usage["cached_prompt_tokens"] = int(getattr(agent, "session_cache_read_tokens", 0) or 0)
        usage["cache_creation_input_tokens"] = int(getattr(agent, "session_cache_write_tokens", 0) or 0)
        usage["reasoning_tokens"] = int(getattr(agent, "session_reasoning_tokens", 0) or 0)
    return usage

def _tyde_make_agent(*args, **kwargs):
    agent = _tyde_original_make_agent(*args, **kwargs)
    tyde_prompt = os.environ.get("TYDE_HERMES_SYSTEM_PROMPT", "").strip()
    if tyde_prompt:
        existing = str(getattr(agent, "ephemeral_system_prompt", "") or "").strip()
        agent.ephemeral_system_prompt = "\n\n".join(
            part for part in (existing, tyde_prompt) if part
        )
    session_id = str(args[0] if args else kwargs.get("sid") or "")
    original_step_callback = getattr(agent, "step_callback", None)
    def _tyde_step_callback(iteration, previous_tools):
        try:
            if callable(original_step_callback):
                original_step_callback(iteration, previous_tools)
        finally:
            if int(iteration or 0) > 1:
                _tyde_original_emit(
                    "provider.request.start",
                    session_id,
                    {"iteration": int(iteration), "usage": _tyde_get_usage(agent)},
                )
    agent.step_callback = _tyde_step_callback
    return agent

def _tyde_emit(event_type, session_id, payload=None):
    if event_type == "message.start":
        with _tyde_message_condition:
            while session_id in _tyde_open_messages:
                _tyde_message_condition.wait()
            _tyde_open_messages.add(session_id)
    if event_type == "tool.start" and isinstance(payload, dict):
        tool_id = str(payload.get("tool_id") or payload.get("tool_call_id") or "")
        args = _tyde_tool_start_args.pop((session_id, tool_id), None)
        if isinstance(args, dict):
            payload = dict(payload)
            payload["args"] = args
    try:
        _tyde_original_emit(event_type, session_id, payload)
    finally:
        if event_type in ("message.complete", "error"):
            with _tyde_message_condition:
                _tyde_open_messages.discard(session_id)
                _tyde_message_condition.notify_all()

_tyde_original_tool_start = _tyde_gateway_server._on_tool_start

def _tyde_on_tool_start(session_id, tool_call_id, name, args):
    if isinstance(args, dict):
        _tyde_tool_start_args[(session_id, str(tool_call_id))] = args
    _tyde_original_tool_start(session_id, tool_call_id, name, args)

_tyde_gateway_server._emit = _tyde_emit
_tyde_gateway_server._get_usage = _tyde_get_usage
_tyde_gateway_server._make_agent = _tyde_make_agent
_tyde_gateway_server._on_tool_start = _tyde_on_tool_start
from tui_gateway.entry import main

if len(sys.argv) > 1:
    os.environ["HERMES_TUI_TOOLSETS"] = sys.argv[1]
main()
"#;
/// Printed by the registration script (and matched by Tyde) when the Hermes
/// install is missing the optional `mcp` Python package. Without it Hermes sets
/// `_MCP_AVAILABLE = False` and silently skips MCP discovery, so it never spawns
/// the bridge and Tyde would otherwise wait out the full startup timeout with an
/// opaque error. Detecting it here turns that 15s hang into an actionable fix.
const HERMES_MCP_MISSING_MARKER: &str = "__TYDE_HERMES_MCP_MISSING__";

/// How many trailing Hermes stderr lines to retain per turn so a message-less
/// gateway exit can still report the underlying cause (e.g. an API-failure
/// panel printed just before the process died).
const HERMES_STDERR_TAIL: usize = 30;

const HERMES_BRIDGE_REGISTRATION: &str = r#"
import importlib.util
import json
import sys

if importlib.util.find_spec("mcp") is None:
    print("__TYDE_HERMES_MCP_MISSING__")
    raise SystemExit(0)

from hermes_cli.config import read_raw_config, save_config

name = sys.argv[1]
command = sys.argv[2]
managed = {"command": command, "args": ["hermes-mcp-bridge"]}
config = read_raw_config()
if not isinstance(config, dict):
    config = {}
servers = config.get("mcp_servers")
if not isinstance(servers, dict):
    servers = {}
    config["mcp_servers"] = servers
existing = servers.get(name)
if existing != managed:
    if existing is not None and not (
        isinstance(existing, dict)
        and existing.get("args") == ["hermes-mcp-bridge"]
    ):
        raise RuntimeError(f"Hermes MCP server name '{name}' is already user-managed")
    servers[name] = managed
    save_config(config)

from tui_gateway.server import _load_enabled_toolsets
selected = _load_enabled_toolsets()
print(json.dumps(selected))
"#;

/// Make Tyde's skill store discoverable by Hermes' own skill loader.
///
/// Hermes discovers skills from `<HERMES_HOME>/skills` plus the directories
/// named by `skills.external_dirs` in that home's `config.yaml`. There is no env
/// var and no per-session flag for it, so registering the store in the profile's
/// config is the only way a Tyde-installed skill can appear in `skills_list` at
/// all — without it, naming those skills in the prompt points the model at files
/// it cannot find.
///
/// Idempotent and additive: entries are compared after `expanduser`, an existing
/// entry is left alone, and every directory the user configured themselves is
/// preserved. `save_config` is only called when something actually changed, so a
/// repeat session does not rewrite the file (Hermes caches this list on
/// `config.yaml`'s mtime).
const HERMES_SKILLS_DIR_REGISTRATION: &str = r#"
import json
import sys
from pathlib import Path

from hermes_cli.config import read_raw_config, save_config

wanted = sys.argv[1]
config = read_raw_config()
if not isinstance(config, dict):
    config = {}
skills = config.get("skills")
if not isinstance(skills, dict):
    skills = {}
    config["skills"] = skills
raw = skills.get("external_dirs")
if isinstance(raw, str):
    raw = [raw]
if not isinstance(raw, list):
    raw = []
existing = {
    str(Path(entry).expanduser()) for entry in raw if isinstance(entry, str)
}
if str(Path(wanted).expanduser()) not in existing:
    raw = list(raw) + [wanted]
    skills["external_dirs"] = raw
    save_config(config)
print(json.dumps(skills.get("external_dirs", [])))
"#;

#[cfg(test)]
static TEST_HERMES_PYTHON: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
#[cfg(test)]
static TEST_HERMES_EXECUTABLE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
#[cfg(test)]
static TEST_HERMES_BRIDGE_EXECUTABLE: std::sync::Mutex<Option<String>> =
    std::sync::Mutex::new(None);
#[cfg(test)]
pub(crate) static TEST_HERMES_OVERRIDE_LOCK: tokio::sync::Mutex<()> =
    tokio::sync::Mutex::const_new(());

#[derive(Clone)]
pub struct HermesBackend {
    command_tx: mpsc::UnboundedSender<HermesBackendCommand>,
    session_id: Arc<std::sync::Mutex<SessionId>>,
    compaction_capability: Arc<std::sync::Mutex<BackendCompactionCapability>>,
    active_compaction:
        Arc<std::sync::Mutex<Option<(protocol::CompactionOperationId, std::time::Instant)>>>,
}

enum HermesBackendCommand {
    Input(AgentInput),
    UpdateSessionSettings(
        protocol::SetSessionSettingsPayload,
        oneshot::Sender<Result<(), String>>,
    ),
    SetSubagentEmitter(Arc<dyn SubAgentEmitter>, oneshot::Sender<()>),
    Interrupt(oneshot::Sender<bool>),
    Compact(
        BackendCompactionRequest,
        oneshot::Sender<BackendCompactionStart>,
    ),
    Shutdown(oneshot::Sender<()>),
}

#[derive(Clone)]
struct HermesGatewayHandle {
    tx: mpsc::UnboundedSender<HermesGatewayCommand>,
    request_timeout: Duration,
    system_overlay_installed: bool,
    provider_version: Option<String>,
    /// The instructions this gateway was started with. Rendered inside `spawn`,
    /// because whether the skills can be named depends on whether registering
    /// the store actually took — and that is only known there.
    spawn_instructions: Option<String>,
    /// Why this session has no Tyde skills, when it was supposed to. Surfaced to
    /// the user by the caller; a session is never refused over it.
    skill_notice: Option<String>,
}

enum HermesGatewayCommand {
    Request {
        method: String,
        params: Value,
        reply: oneshot::Sender<Result<Value, HermesRpcError>>,
        dispatched: Option<oneshot::Sender<Result<(), HermesRpcError>>>,
    },
    Shutdown(oneshot::Sender<()>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HermesRpcError {
    code: Option<i64>,
    message: String,
}

enum HermesDispatchError {
    NotSent,
    Uncertain(HermesRpcError),
}

impl std::fmt::Display for HermesRpcError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.code {
            Some(code) => write!(formatter, "Hermes JSON-RPC error {code}: {}", self.message),
            None => formatter.write_str(&self.message),
        }
    }
}

enum HermesGatewayInbound {
    StdoutLine(String),
    StderrLine(String),
    Closed(Option<i32>),
}

#[derive(Debug)]
enum HermesGatewayEvent {
    Event {
        event_type: String,
        session_id: Option<String>,
        payload: Option<Value>,
    },
    ProtocolError(String),
    Stderr(String),
    Closed(Option<i32>),
}

struct HermesSpawnTarget {
    program: String,
    args: Vec<String>,
    env: HashMap<String, String>,
    cwd: Option<String>,
    remote_host: Option<String>,
    display_program: String,
    provider_version: Option<String>,
}

struct HermesMcpRuntime {
    _descriptor_dir: tempfile::TempDir,
    ready_path: PathBuf,
}

pub(crate) struct HermesCliGatewayProbe {
    pub(crate) executable: String,
    pub(crate) gateway_python: String,
    pub(crate) version: Option<String>,
}

#[derive(Debug, Clone)]
struct HermesGatewayPythonCandidate {
    program: String,
    source: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HermesProbeFailure {
    pub(crate) code: BackendSetupDiagnosticCode,
    pub(crate) message: String,
}

impl HermesProbeFailure {
    fn new(code: BackendSetupDiagnosticCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub(crate) fn explicit_override(mut self, variable: &str) -> Self {
        self.message = format!("{variable} override is invalid: {}", self.message);
        self
    }
}

impl std::fmt::Display for HermesProbeFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.message.fmt(f)
    }
}

struct HermesVersionOutput {
    stdout: String,
    stderr: String,
}

fn hermes_compaction_capability(version: Option<&str>) -> BackendCompactionCapability {
    BackendCompactionCapability::native(
        BackendCompactionMechanism::JsonRpcRequest,
        version
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned),
        BackendCompactionCapabilityEvidence::HermesMethodProbe,
    )
}

fn hermes_compaction_pre_dispatch(
    capability: &BackendCompactionCapability,
    transcript_authoritative: bool,
) -> Option<BackendCompactionStart> {
    if !transcript_authoritative {
        return Some(BackendCompactionStart::NotDispatched {
            reason: BackendCompactionNotDispatchedReason::NativeUnavailable(
                BackendCompactionUnavailableReason::TranscriptNotAuthoritative,
            ),
            fallback_safe: true,
        });
    }
    crate::backend::compaction::not_dispatched_for_capability(capability)
}

fn hermes_dispatch_uncertain_result(
    operation_id: protocol::CompactionOperationId,
    live_session_id: String,
    stored_session_id: SessionId,
    error: HermesRpcError,
) -> BackendCompactionResult {
    BackendCompactionResult {
        operation_id,
        dispatch: BackendCompactionDispatchState::MayHaveReachedProvider,
        mutation: BackendCompactionMutationState::MayHaveMutated,
        outcome: Err(BackendCompactionFailure {
            kind: BackendCompactionFailureKind::TransportClosed,
            message: error.to_string(),
        }),
        provider_session_id: Some(stored_session_id.clone()),
        metrics: CompactionMetrics::default(),
        post_context_tokens: PostCompactionTokenCount::Unknown,
        evidence: BackendCompactionTerminalEvidence::Hermes {
            live_session_id,
            stored_session_id: stored_session_id.0,
            response_status: None,
            rpc_code: error.code,
        },
    }
}

fn classify_hermes_compaction_response(
    operation_id: protocol::CompactionOperationId,
    live_session_id: String,
    stored_before: SessionId,
    response: Result<Value, HermesRpcError>,
    stored_session_id: &Arc<std::sync::Mutex<SessionId>>,
    compaction_capability: &Arc<std::sync::Mutex<BackendCompactionCapability>>,
) -> BackendCompactionResult {
    match response {
        Ok(value) => {
            let status = optional_string(&value, &["status"]);
            let info = value.get("info").filter(|info| info.is_object());
            let new_stored = info
                .and_then(|info| {
                    optional_string_any(info, &["resumed", "session_key", "stored_session_id"])
                })
                .or_else(|| {
                    optional_string_any(&value, &["resumed", "session_key", "stored_session_id"])
                })
                .map(SessionId)
                .unwrap_or_else(|| stored_before.clone());
            let metrics = CompactionMetrics {
                before_tokens: value.get("before_tokens").and_then(Value::as_u64),
                after_tokens: value.get("after_tokens").and_then(Value::as_u64),
                before_messages: value.get("before_messages").and_then(Value::as_u64),
                after_messages: value.get("after_messages").and_then(Value::as_u64),
                messages_summarized: value
                    .get("removed")
                    .or_else(|| value.get("messages_summarized"))
                    .and_then(Value::as_u64),
                duration_ms: value.get("duration_ms").and_then(Value::as_u64),
                precomputed: value.get("precomputed").and_then(Value::as_bool),
                ..CompactionMetrics::default()
            };
            let completed = status.as_deref() == Some("compressed")
                && metrics.before_tokens.is_some()
                && metrics.after_tokens.is_some()
                && metrics.before_messages.is_some()
                && metrics.after_messages.is_some();
            if completed {
                *stored_session_id
                    .lock()
                    .expect("Hermes stored session id mutex poisoned") = new_stored.clone();
            }
            BackendCompactionResult {
                operation_id,
                dispatch: BackendCompactionDispatchState::Accepted,
                mutation: if completed {
                    BackendCompactionMutationState::Completed
                } else {
                    BackendCompactionMutationState::MayHaveMutated
                },
                outcome: if completed {
                    Ok(BackendCompactionSuccess {
                        mechanism: CompactionMethod::NativeRpc,
                    })
                } else {
                    Err(BackendCompactionFailure {
                        kind: BackendCompactionFailureKind::ProtocolViolation,
                        message: format!(
                            "Hermes session.compress returned an unvalidated terminal response (status {:?})",
                            status,
                        ),
                    })
                },
                provider_session_id: Some(new_stored.clone()),
                post_context_tokens: metrics
                    .after_tokens
                    .map(PostCompactionTokenCount::Trusted)
                    .unwrap_or(PostCompactionTokenCount::Unknown),
                metrics,
                evidence: BackendCompactionTerminalEvidence::Hermes {
                    live_session_id,
                    stored_session_id: new_stored.0,
                    response_status: status,
                    rpc_code: None,
                },
            }
        }
        Err(error) => {
            if error.code == Some(-32601) {
                let previous = compaction_capability
                    .lock()
                    .expect("Hermes compaction capability mutex poisoned")
                    .clone();
                *compaction_capability
                    .lock()
                    .expect("Hermes compaction capability mutex poisoned") =
                    BackendCompactionCapability::context_unavailable_with_metadata(
                        BackendCompactionUnavailableReason::ManualTriggerAbsent,
                        previous.provider_version,
                        previous.evidence,
                    );
            }
            BackendCompactionResult {
                operation_id,
                dispatch: if error.code == Some(-32601) {
                    BackendCompactionDispatchState::Rejected
                } else if error.code.is_some() {
                    BackendCompactionDispatchState::Accepted
                } else {
                    BackendCompactionDispatchState::MayHaveReachedProvider
                },
                mutation: if matches!(error.code, Some(4009) | Some(-32601)) {
                    BackendCompactionMutationState::NotObserved
                } else {
                    BackendCompactionMutationState::MayHaveMutated
                },
                outcome: Err(BackendCompactionFailure {
                    kind: if error.message.contains("timed out") {
                        BackendCompactionFailureKind::TimedOut
                    } else if matches!(error.code, Some(-32601) | Some(4009)) {
                        BackendCompactionFailureKind::ProviderRejected
                    } else if error.code.is_some() {
                        BackendCompactionFailureKind::ProviderFailed
                    } else {
                        BackendCompactionFailureKind::TransportClosed
                    },
                    message: error.to_string(),
                }),
                provider_session_id: Some(stored_before.clone()),
                metrics: CompactionMetrics::default(),
                post_context_tokens: PostCompactionTokenCount::Unknown,
                evidence: BackendCompactionTerminalEvidence::Hermes {
                    live_session_id,
                    stored_session_id: stored_before.0,
                    response_status: None,
                    rpc_code: error.code,
                },
            }
        }
    }
}

struct HermesSessionIds {
    live_session_id: String,
    stored_session_id: SessionId,
}

struct HermesSessionActor {
    gateway: HermesGatewayHandle,
    live_session_id: String,
    mapper: HermesEventMapper,
    events_tx: mpsc::UnboundedSender<BackendEvent>,
    stored_session_id: Arc<std::sync::Mutex<SessionId>>,
    compaction_capability: Arc<std::sync::Mutex<BackendCompactionCapability>>,
    active_compaction:
        Arc<std::sync::Mutex<Option<(protocol::CompactionOperationId, std::time::Instant)>>>,
    command_rx: mpsc::UnboundedReceiver<HermesBackendCommand>,
    gateway_events_rx: mpsc::UnboundedReceiver<HermesGatewayEvent>,
    subagent_emitter: Option<Arc<dyn SubAgentEmitter>>,
    native_subagents: HashMap<String, HermesNativeSubagent>,
    /// Base synthetic id → (current issued id, generation) for id-less native
    /// children, so a reissued base never hands a new child a finished
    /// child's identity.
    synthetic_subagent_ids: HashMap<String, (String, u64)>,
    /// Bounded tail of the most recent Hermes stderr lines for the current turn.
    /// Stderr is normally diagnostic-only (logged at debug), but if a turn dies
    /// without a protocol-level message (e.g. the gateway exits mid-call after
    /// exhausting API retries) this tail is attached to the failure so the real
    /// cause is never silenced.
    recent_stderr: VecDeque<String>,
}

struct HermesNativeSubagent {
    handle: SubAgentHandle,
    agent_name: String,
    /// `None` when no delegation card could be attributed unambiguously; the
    /// child's progress then stays unanchored instead of landing on another
    /// tool's card.
    parent_anchor: Option<HermesDelegationAnchor>,
    tool_calls: u64,
}

#[derive(Clone)]
struct HermesTurnTool {
    name: String,
    content_offset: Option<u32>,
    observed_order: u64,
}

#[derive(Default)]
struct HermesEventMapper {
    current_message_id: Option<String>,
    current_text: String,
    current_reasoning_seen: bool,
    model: Option<String>,
    provider: Option<String>,
    pending_tools: HashMap<String, String>,
    pending_tool_arguments: HashMap<String, Value>,
    turn_tools: HashMap<String, HermesTurnTool>,
    next_turn_tool_order: u64,
    cancelled_tools: HashSet<String>,
    background_tasks: HashMap<String, HermesBackgroundTask>,
    pending_approval_tool_id: Option<String>,
    last_session_usage: Option<TokenUsage>,
    cumulative_usage_incomplete: bool,
    awaiting_interrupted_complete: bool,
    session_info_emitted: bool,
    approval_counter: u64,
    normalization_failures: HashMap<String, PendingToolNormalizationFailure>,
    delegation_tools: VecDeque<HermesDelegationTool>,
    task_ids: HashMap<String, u64>,
    next_task_id: u64,
}

#[derive(Clone)]
struct HermesDelegationTool {
    tool_call_id: String,
    /// Raw gateway name from the emitted `ToolRequest` (the authority for
    /// this `tool_call_id`), carried into every progress frame.
    tool_name: String,
    goals: Vec<String>,
}

impl HermesDelegationTool {
    fn anchor(&self) -> HermesDelegationAnchor {
        HermesDelegationAnchor {
            tool_call_id: self.tool_call_id.clone(),
            tool_name: self.tool_name.clone(),
        }
    }
}

#[derive(Clone)]
struct HermesDelegationAnchor {
    tool_call_id: String,
    tool_name: String,
}

#[derive(Clone)]
struct HermesBackgroundTask {
    tool_call_id: String,
    /// Raw gateway name from the emitted `ToolRequest` (the authority for
    /// this `tool_call_id`), carried into every progress frame.
    tool_name: String,
    command: Option<String>,
}

pub(crate) fn resolve_session_settings(config: &BackendSpawnConfig) -> SessionSettingsValues {
    resolve_backend_settings(
        config,
        &HermesBackend::session_settings_schema(),
        hermes_cost_hint_defaults,
    )
}

fn hermes_cost_hint_defaults(_cost_hint: protocol::SpawnCostHint) -> SessionSettingsValues {
    SessionSettingsValues::default()
}

fn hermes_base_session_fields() -> Vec<SessionSettingField> {
    vec![
        SessionSettingField {
            key: "reasoning_effort".to_string(),
            label: "Reasoning Effort".to_string(),
            description: Some(
                "Per-session Hermes reasoning effort; Auto uses the Hermes profile default."
                    .to_string(),
            ),
            use_slider: false,
            select_options_by_setting: None,
            field_type: SessionSettingFieldType::Select {
                options: vec![
                    SelectOption {
                        value: "none".to_string(),
                        label: "None".to_string(),
                    },
                    SelectOption {
                        value: "minimal".to_string(),
                        label: "Minimal".to_string(),
                    },
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
                        value: "xhigh".to_string(),
                        label: "XHigh".to_string(),
                    },
                ],
                default: None,
                nullable: true,
            },
        },
        SessionSettingField {
            key: "fast".to_string(),
            label: "Fast Mode".to_string(),
            description: Some("Request Hermes fast service tier when available.".to_string()),
            use_slider: false,
            select_options_by_setting: None,
            field_type: SessionSettingFieldType::Toggle { default: false },
        },
    ]
}

impl Backend for HermesBackend {
    fn capabilities() -> tyde_agent_adapter::BackendCapabilities {
        [
            tyde_agent_adapter::BackendCapability::ListSessions,
            tyde_agent_adapter::BackendCapability::ResumeSession,
            tyde_agent_adapter::BackendCapability::Interrupt,
            tyde_agent_adapter::BackendCapability::SessionSettings,
            tyde_agent_adapter::BackendCapability::StartupMcpServers,
            tyde_agent_adapter::BackendCapability::TurnUsageReported,
            tyde_agent_adapter::BackendCapability::ContextUsageReported,
            tyde_agent_adapter::BackendCapability::Subagents,
            tyde_agent_adapter::BackendCapability::BackgroundTasks,
            tyde_agent_adapter::BackendCapability::WorkspaceInstructions,
            tyde_agent_adapter::BackendCapability::Customization,
        ]
        .into()
    }

    fn session_settings_schema() -> SessionSettingsSchema {
        SessionSettingsSchema {
            backend_kind: BackendKind::Hermes,
            fields: hermes_base_session_fields(),
        }
    }

    async fn spawn(
        workspace_roots: Vec<String>,
        config: BackendSpawnConfig,
        initial_input: protocol::SendMessagePayload,
    ) -> Result<(Self, EventStream), String> {
        reject_unverified_capabilities(&config, &initial_input)?;
        let resolved_settings = resolve_session_settings(&config);
        let profile = resolve_session_profile(&resolved_settings)?;
        let expects_mcp_tools = !config.startup_mcp_servers.is_empty();
        let remote_host =
            crate::remote::parse_remote_workspace_roots(&workspace_roots)?.map(|(host, _)| host);
        let dropped_skills_notice =
            hermes_remote_skill_notice(&config.resolved_spawn_config, remote_host.as_deref());
        let expose_skills =
            remote_host.is_none() && !config.resolved_spawn_config.skills.is_empty();
        let (gateway, mut gateway_events_rx) = HermesGatewayHandle::spawn(
            &workspace_roots,
            &config.startup_mcp_servers,
            &config.resolved_spawn_config.tool_policy,
            &profile,
            &config.resolved_spawn_config,
            expose_skills,
        )
        .await?;
        let spawn_instructions = gateway.spawn_instructions.clone();
        // Either the store could not travel, or registering it did not take.
        // Both are notices; neither stops the session.
        let dropped_skills_notice = dropped_skills_notice.or_else(|| gateway.skill_notice.clone());
        let history_instructions = (!gateway.system_overlay_installed)
            .then_some(spawn_instructions.as_deref())
            .flatten();
        let create_params = build_session_create_params(
            &workspace_roots,
            &resolved_settings,
            history_instructions,
        )?;
        let create = gateway.request("session.create", create_params).await?;
        let ids = parse_session_create_ids(&create)?;
        let startup_gateway_events = if expects_mcp_tools {
            match wait_for_hermes_session_mcp_tools(&mut gateway_events_rx, &ids.live_session_id)
                .await
            {
                Ok(events) => events,
                Err(error) => {
                    gateway.shutdown().await;
                    return Err(error);
                }
            }
        } else {
            Vec::new()
        };
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        // A Default session that could not bring its skills says so. The channel
        // is unbounded and its receiver is returned below, so this survives
        // until the client attaches.
        if let Some(notice) = dropped_skills_notice {
            let _ = events_tx.send(BackendEvent::Chat(ChatEvent::MessageAdded(
                warning_message(notice),
            )));
        }

        let stored_session_id = Arc::new(std::sync::Mutex::new(ids.stored_session_id));
        let compaction_capability = Arc::new(std::sync::Mutex::new(hermes_compaction_capability(
            gateway.provider_version.as_deref(),
        )));
        let active_compaction = Arc::new(std::sync::Mutex::new(None));
        let actor = HermesSessionActor {
            gateway: gateway.clone(),
            live_session_id: ids.live_session_id.clone(),
            mapper: HermesEventMapper::default(),
            events_tx,
            stored_session_id: Arc::clone(&stored_session_id),
            compaction_capability: Arc::clone(&compaction_capability),
            active_compaction: Arc::clone(&active_compaction),
            command_rx,
            gateway_events_rx,
            subagent_emitter: None,
            native_subagents: HashMap::new(),
            synthetic_subagent_ids: HashMap::new(),
            recent_stderr: VecDeque::new(),
        };
        tokio::spawn(actor.run(Some(initial_input), None, startup_gateway_events));

        Ok((
            Self {
                command_tx,
                session_id: stored_session_id,
                compaction_capability,
                active_compaction,
            },
            EventStream::new_backend(events_rx),
        ))
    }

    async fn resume(
        workspace_roots: Vec<String>,
        config: BackendSpawnConfig,
        session_id: SessionId,
    ) -> Result<(Self, EventStream), String> {
        reject_unverified_resume_capabilities(&config)?;
        let resolved_settings = resolve_session_settings(&config);
        let profile = resolve_session_profile(&resolved_settings)?;
        let remote_host =
            crate::remote::parse_remote_workspace_roots(&workspace_roots)?.map(|(host, _)| host);
        // Resume refuses any session whose instructions cannot be installed
        // remotely (below), which is a different question from whether the
        // skills came along; the notice for those is surfaced with the rest.
        let dropped_skills_notice =
            hermes_remote_skill_notice(&config.resolved_spawn_config, remote_host.as_deref());
        let expose_skills =
            remote_host.is_none() && !config.resolved_spawn_config.skills.is_empty();
        let (gateway, gateway_events_rx) = HermesGatewayHandle::spawn(
            &workspace_roots,
            &config.startup_mcp_servers,
            &config.resolved_spawn_config.tool_policy,
            &profile,
            &config.resolved_spawn_config,
            expose_skills,
        )
        .await?;
        let spawn_instructions = gateway.spawn_instructions.clone();
        let dropped_skills_notice = dropped_skills_notice.or_else(|| gateway.skill_notice.clone());
        if spawn_instructions.is_some() && !gateway.system_overlay_installed {
            gateway.shutdown().await;
            return Err(
                "Hermes cannot safely resume this remote session because the remote gateway \
                 cannot restore Tyde system instructions"
                    .to_string(),
            );
        }
        let resume = gateway
            .request(
                "session.resume",
                json!({
                    "session_id": session_id.0,
                    "cols": 80,
                    "eager_build": false,
                    "source": "tyde",
                }),
            )
            .await?;
        let live_session_id = required_string(&resume, &["session_id"], "session.resume")?;
        let resumed = optional_string(&resume, &["resumed"])
            .or_else(|| optional_string(&resume, &["session_key"]))
            .unwrap_or_else(|| session_id.0.clone());
        if resumed != session_id.0 {
            tracing::info!(from = %session_id.0, to = %resumed, "Hermes resume resolved continuation session");
        }
        let history = gateway
            .request("session.history", json!({ "session_id": live_session_id }))
            .await?;
        let replay_events = hermes_history_to_chat_events(&history)?;
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        // A resumed session that came back without its skills says so, exactly
        // as a fresh one does. The channel is unbounded and its receiver is
        // returned below, so this survives until the client attaches.
        if let Some(notice) = dropped_skills_notice {
            let _ = events_tx.send(BackendEvent::Chat(ChatEvent::MessageAdded(
                warning_message(notice),
            )));
        }
        let (resume_replay_complete_tx, resume_replay_complete_rx) = oneshot::channel();
        let stored_session_id = Arc::new(std::sync::Mutex::new(SessionId(resumed)));
        let compaction_capability = Arc::new(std::sync::Mutex::new(hermes_compaction_capability(
            gateway.provider_version.as_deref(),
        )));
        let active_compaction = Arc::new(std::sync::Mutex::new(None));
        let actor = HermesSessionActor {
            gateway: gateway.clone(),
            live_session_id,
            mapper: HermesEventMapper {
                cumulative_usage_incomplete: true,
                ..HermesEventMapper::default()
            },
            events_tx,
            stored_session_id: Arc::clone(&stored_session_id),
            compaction_capability: Arc::clone(&compaction_capability),
            active_compaction: Arc::clone(&active_compaction),
            command_rx,
            gateway_events_rx,
            subagent_emitter: None,
            native_subagents: HashMap::new(),
            synthetic_subagent_ids: HashMap::new(),
            recent_stderr: VecDeque::new(),
        };
        tokio::spawn(actor.run(
            None,
            Some((replay_events, resume_replay_complete_tx)),
            Vec::new(),
        ));

        Ok((
            Self {
                command_tx,
                session_id: stored_session_id,
                compaction_capability,
                active_compaction,
            },
            EventStream::new_backend_with_resume_replay_barrier(
                events_rx,
                resume_replay_complete_rx,
            ),
        ))
    }

    async fn fork(
        _workspace_roots: Vec<String>,
        _config: BackendSpawnConfig,
        _from_session_id: SessionId,
        _initial_input: protocol::SendMessagePayload,
    ) -> Result<(Self, EventStream), BackendStartupError> {
        Err(BackendStartupError::unsupported(
            backend_fork_unsupported_message(BackendKind::Hermes),
        ))
    }

    async fn list_sessions() -> Result<Vec<BackendSession>, String> {
        let profile = hermes_config::resolve_profile_ref(None)?;
        let (gateway, _gateway_events_rx) = HermesGatewayHandle::spawn(
            &[],
            &[],
            &protocol::ToolPolicy::Unrestricted,
            &profile,
            &ResolvedSpawnConfig::default(),
            false,
        )
        .await?;
        let result = gateway
            .request("session.list", json!({ "limit": 200 }))
            .await;
        let resumable = gateway.system_overlay_installed;
        gateway.shutdown().await;
        parse_session_list(&result?, resumable)
    }

    fn session_id(&self) -> SessionId {
        self.session_id
            .lock()
            .expect("Hermes stored session id mutex poisoned")
            .clone()
    }

    fn compaction_capability(&self) -> BackendCompactionCapability {
        self.compaction_capability
            .lock()
            .expect("Hermes compaction capability mutex poisoned")
            .clone()
    }

    async fn begin_compaction(&self, request: BackendCompactionRequest) -> BackendCompactionStart {
        let capability = self
            .compaction_capability
            .lock()
            .expect("Hermes compaction capability mutex poisoned")
            .clone();
        if let Some(start) =
            hermes_compaction_pre_dispatch(&capability, request.transcript_authoritative)
        {
            return start;
        }
        if self
            .active_compaction
            .lock()
            .expect("Hermes active compaction mutex poisoned")
            .is_some()
        {
            return BackendCompactionStart::Deferred {
                reason: BackendCompactionDeferredReason::AnotherCompactionActive,
            };
        }
        let (reply_tx, reply_rx) = oneshot::channel();
        if self
            .command_tx
            .send(HermesBackendCommand::Compact(request, reply_tx))
            .is_err()
        {
            return BackendCompactionStart::NotDispatched {
                reason: BackendCompactionNotDispatchedReason::BackendClosed,
                fallback_safe: false,
            };
        }
        reply_rx
            .await
            .unwrap_or(BackendCompactionStart::NotDispatched {
                reason: BackendCompactionNotDispatchedReason::BackendClosed,
                fallback_safe: false,
            })
    }

    async fn send(&self, input: AgentInput) -> bool {
        match input {
            AgentInput::SendMessage(_) | AgentInput::UpdateSessionSettings(_) => self
                .command_tx
                .send(HermesBackendCommand::Input(input))
                .is_ok(),
            AgentInput::EditQueuedMessage(_)
            | AgentInput::CancelQueuedMessage(_)
            | AgentInput::SendQueuedMessageNow(_) => {
                tracing::error!("queued-message inputs reached Hermes backend");
                false
            }
        }
    }

    async fn update_session_settings(
        &mut self,
        payload: protocol::SetSessionSettingsPayload,
    ) -> Result<(), String> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.command_tx
            .send(HermesBackendCommand::UpdateSessionSettings(
                payload, reply_tx,
            ))
            .map_err(|_| "Hermes terminated before applying session settings".to_owned())?;
        reply_rx
            .await
            .map_err(|_| "Hermes terminated while applying session settings".to_owned())?
    }

    async fn interrupt(&self) -> bool {
        let (reply_tx, reply_rx) = oneshot::channel();
        if self
            .command_tx
            .send(HermesBackendCommand::Interrupt(reply_tx))
            .is_err()
        {
            return false;
        }
        match tokio::time::timeout(Duration::from_secs(5), reply_rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) | Err(_) => false,
        }
    }

    async fn shutdown(self) {
        let (reply_tx, reply_rx) = oneshot::channel();
        if self
            .command_tx
            .send(HermesBackendCommand::Shutdown(reply_tx))
            .is_ok()
        {
            let _ = tokio::time::timeout(HERMES_SHUTDOWN_TIMEOUT, reply_rx).await;
        }
    }
}

impl HermesBackend {
    pub(crate) async fn set_subagent_emitter(&self, emitter: Arc<dyn SubAgentEmitter>) {
        let (reply_tx, reply_rx) = oneshot::channel();
        if self
            .command_tx
            .send(HermesBackendCommand::SetSubagentEmitter(emitter, reply_tx))
            .is_ok()
        {
            let _ = reply_rx.await;
        }
    }
}

/// The session-setting key that selects a Hermes profile for the session's
/// gateway (`"default"` or a `~/.hermes/profiles/<name>` directory name).
pub(crate) const HERMES_PROFILE_SETTING: &str = "profile";

/// A running Hermes session's gateway is bound to the profile's HERMES_HOME
/// it was spawned with; the profile cannot change mid-session.
pub(crate) fn validate_runtime_session_settings_update(
    current: &SessionSettingsValues,
    update: &SessionSettingsValues,
) -> Result<(), String> {
    if let Some(requested) = update.0.get(HERMES_PROFILE_SETTING)
        && normalized_profile_setting(Some(requested))
            != normalized_profile_setting(current.0.get(HERMES_PROFILE_SETTING))
    {
        return Err(
            "Hermes profile cannot be changed on a running session; start a new Hermes session \
             with the desired profile"
                .to_string(),
        );
    }
    Ok(())
}

fn normalized_profile_setting(value: Option<&SessionSettingValue>) -> Option<&str> {
    match value {
        Some(SessionSettingValue::String(name)) if !name.trim().is_empty() => Some(name.trim()),
        Some(SessionSettingValue::Null) | None => Some(hermes_config_default_profile()),
        Some(SessionSettingValue::String(_))
        | Some(SessionSettingValue::Bool(_))
        | Some(SessionSettingValue::Integer(_)) => None,
    }
}

fn resolve_session_profile(settings: &SessionSettingsValues) -> Result<HermesProfileRef, String> {
    let name = match settings.0.get(HERMES_PROFILE_SETTING) {
        Some(SessionSettingValue::String(name)) => Some(name.as_str()),
        Some(SessionSettingValue::Null) | None => None,
        Some(other) => {
            return Err(format!(
                "Hermes profile session setting must be a string, found {other:?}"
            ));
        }
    };
    hermes_config::resolve_profile_ref(name)
}

/// Probe one profile's gateway for its `model.options` payload.
pub(crate) async fn probe_model_options(
    workspace_roots: &[String],
    profile: &HermesProfileRef,
) -> Result<Value, String> {
    probe_profile_surfaces(workspace_roots, profile).await.0
}

/// Probe one profile's gateway for everything the settings page needs, on a
/// single gateway spawn. Spawning a Hermes gateway is the expensive part of a
/// snapshot (one per profile), so the toolset catalogue rides along with the
/// model options rather than paying for a second one.
///
/// The two results are independent: a failed `toolsets.list` leaves the
/// provider list intact and vice versa, because each only disables the control
/// it feeds.
pub(crate) async fn probe_profile_surfaces(
    workspace_roots: &[String],
    profile: &HermesProfileRef,
) -> (Result<Value, String>, Result<Value, String>) {
    let spawned = HermesGatewayHandle::spawn(
        workspace_roots,
        &[],
        &protocol::ToolPolicy::Unrestricted,
        profile,
        &ResolvedSpawnConfig::default(),
        false,
    )
    .await;
    let (gateway, _events) = match spawned {
        Ok(spawned) => spawned,
        Err(error) => return (Err(error.clone()), Err(error)),
    };
    let options = gateway.request("model.options", json!({})).await;
    let toolsets = gateway.request("toolsets.list", json!({})).await;
    gateway.shutdown().await;
    (options, toolsets)
}

/// Build the Hermes backend-native settings snapshot: every discovered
/// profile with its editable `config.yaml` projection plus live provider
/// states probed from that profile's gateway. A failed provider probe is
/// reported per-profile so config editing stays available; a broken profile
/// config or failed discovery makes the whole snapshot visibly unavailable.
pub(crate) async fn native_settings_snapshot(
    workspace_roots: &[String],
) -> BackendNativeSettingsSnapshot {
    match native_settings_doc(workspace_roots).await {
        Ok(doc) => match serde_json::to_value(&doc) {
            Ok(settings) => BackendNativeSettingsSnapshot {
                backend_kind: BackendKind::Hermes,
                status: BackendConfigSnapshotStatus::Ready,
                settings: Some(settings),
                groups: Vec::new(),
                message: None,
                advisories: Vec::new(),
            },
            Err(error) => hermes_native_settings_unavailable(format!(
                "failed to serialize Hermes settings snapshot: {error}"
            )),
        },
        Err(error) => hermes_native_settings_unavailable(error),
    }
}

fn hermes_native_settings_unavailable(message: String) -> BackendNativeSettingsSnapshot {
    BackendNativeSettingsSnapshot {
        backend_kind: BackendKind::Hermes,
        status: BackendConfigSnapshotStatus::Unavailable,
        settings: None,
        groups: Vec::new(),
        message: Some(message),
        advisories: Vec::new(),
    }
}

async fn native_settings_doc(
    workspace_roots: &[String],
) -> Result<protocol::hermes_config::HermesNativeSettingsDoc, String> {
    use protocol::hermes_config::{HERMES_NATIVE_SETTINGS_VERSION, HermesProfileSettings};

    let profiles = hermes_config::discover_profiles()?;
    let probes = futures_util::future::join_all(
        profiles
            .iter()
            .map(|profile| probe_profile_surfaces(workspace_roots, profile)),
    )
    .await;

    let mut doc = protocol::hermes_config::HermesNativeSettingsDoc {
        version: HERMES_NATIVE_SETTINGS_VERSION,
        profiles: Vec::new(),
        profile_actions: Vec::new(),
        actions: Vec::new(),
    };
    for (profile, (options, toolsets)) in profiles.iter().zip(probes) {
        let config = hermes_config::load_profile_config(&profile.home_dir)?;
        let mut settings = HermesProfileSettings {
            name: profile.name.clone(),
            home_dir: profile.home_dir.to_string_lossy().to_string(),
            config,
            base_config: None,
            providers: None,
            providers_error: None,
            active_model: None,
            active_provider: None,
            toolsets: None,
        };
        match options.and_then(|payload| {
            provider_states_from_payload(&payload).map(|providers| (payload, providers))
        }) {
            Ok((payload, providers)) => {
                settings.active_model =
                    optional_string(&payload, &["model"]).filter(|model| !model.trim().is_empty());
                settings.active_provider = optional_string(&payload, &["provider"])
                    .filter(|provider| !provider.trim().is_empty());
                settings.providers = Some(providers);
            }
            Err(error) => settings.providers_error = Some(error),
        }
        // A toolset probe failure is not surfaced as an error: the catalogue
        // only upgrades the disabled-toolsets control from free text to a
        // picker, so losing it degrades that one control instead of the page.
        settings.toolsets = toolsets
            .ok()
            .and_then(|payload| toolset_infos_from_payload(&payload));
        doc.profiles.push(settings);
    }
    Ok(doc)
}

/// Parse a `toolsets.list` payload. Returns `None` when the payload is not the
/// expected shape, so a Hermes that changes it degrades the control instead of
/// rendering a half-parsed catalogue as if it were complete.
fn toolset_infos_from_payload(
    value: &Value,
) -> Option<Vec<protocol::hermes_config::HermesToolsetInfo>> {
    let toolsets = value.get("toolsets")?.as_array()?;
    let mut infos = Vec::with_capacity(toolsets.len());
    for toolset in toolsets {
        let name = toolset.get("name")?.as_str()?.trim().to_owned();
        if name.is_empty() {
            return None;
        }
        infos.push(protocol::hermes_config::HermesToolsetInfo {
            name,
            description: optional_string(toolset, &["description"])
                .filter(|text| !text.trim().is_empty()),
            tool_count: toolset
                .get("tool_count")
                .and_then(Value::as_u64)
                .unwrap_or(0) as u32,
        });
    }
    Some(infos)
}

fn provider_states_from_payload(
    value: &Value,
) -> Result<Vec<protocol::hermes_config::HermesProviderState>, String> {
    let providers = value
        .get("providers")
        .and_then(Value::as_array)
        .ok_or_else(|| "Hermes model.options response missing providers array".to_string())?;
    let mut states = Vec::new();
    for (index, provider) in providers.iter().enumerate() {
        let context = format!("model.options providers[{index}]");
        let slug = required_non_empty_string(provider, &["slug"], &context)?;
        let name = optional_string(provider, &["name"]).unwrap_or_else(|| slug.clone());
        let authenticated = provider
            .get("authenticated")
            .and_then(Value::as_bool)
            .ok_or_else(|| format!("{context}.authenticated must be a bool"))?;
        // Hermes reports a provider's curated model ids here; carrying them
        // through is what lets the default-model and fallback controls be
        // dropdowns. Non-string entries are skipped rather than stringified —
        // an id Tyde invented would be saved and then rejected by Hermes.
        let models: Vec<String> = provider
            .get("models")
            .and_then(Value::as_array)
            .map(|models| {
                models
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::trim)
                    .filter(|model| !model.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        states.push(protocol::hermes_config::HermesProviderState {
            slug,
            name,
            authenticated,
            auth_type: optional_string(provider, &["auth_type"]),
            key_env: optional_string(provider, &["key_env"]),
            warning: optional_string(provider, &["warning"]),
            model_count: models.len() as u32,
            models,
        });
    }
    Ok(states)
}

/// Apply a client save of the Hermes native settings document: run its
/// credential actions against each profile's gateway, then write changed
/// per-profile config projections back to disk. The document (which may
/// carry an API key inside an action) is never logged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HermesNativeSettingsSaveOutcome {
    Complete,
    Partial { credential_errors: Vec<String> },
}

impl HermesNativeSettingsSaveOutcome {
    pub(crate) fn partial_error_message(&self) -> Option<String> {
        match self {
            Self::Complete => None,
            Self::Partial { credential_errors } => Some(format!(
                "Hermes saved the unrelated configuration changes, but credential actions \
                 failed: {}",
                credential_errors.join("; ")
            )),
        }
    }
}

pub(crate) async fn persist_native_settings(
    settings: Value,
    workspace_roots: &[String],
) -> Result<HermesNativeSettingsSaveOutcome, String> {
    use protocol::hermes_config::{
        HERMES_NATIVE_SETTINGS_VERSION, HermesCredentialAction, HermesNativeSettingsDoc,
        HermesProfileAction,
    };

    let doc: HermesNativeSettingsDoc = serde_json::from_value(settings)
        .map_err(|error| format!("invalid Hermes settings document: {error}"))?;
    if doc.version != HERMES_NATIVE_SETTINGS_VERSION {
        return Err(format!(
            "unsupported Hermes settings document version {} (expected {})",
            doc.version, HERMES_NATIVE_SETTINGS_VERSION
        ));
    }

    // Validate every profile section before any credential or config
    // mutation: a bad document must be rejected whole, not half-applied and
    // rediscovered later as a broken snapshot.
    for profile_settings in &doc.profiles {
        hermes_config::validate_profile_config(&profile_settings.config).map_err(|error| {
            format!(
                "invalid Hermes settings for profile '{}': {error}",
                profile_settings.name
            )
        })?;
    }

    // A deleted profile's own config section is meaningless — the directory
    // holding it is about to be removed — so collect the targets up front and
    // skip them below instead of resolving a profile that will not exist.
    let deleted_profiles: Vec<&str> = doc
        .profile_actions
        .iter()
        .filter_map(|action| match action {
            HermesProfileAction::DeleteProfile { name } => Some(name.as_str()),
            HermesProfileAction::CreateProfile { .. } => None,
        })
        .collect();
    for action in &doc.actions {
        let profile_name = match action {
            HermesCredentialAction::SaveApiKey { profile, .. }
            | HermesCredentialAction::Disconnect { profile, .. } => profile.as_str(),
        };
        if deleted_profiles.contains(&profile_name) {
            return Err(format!(
                "cannot change credentials for Hermes profile '{profile_name}' in the same \
                 save that deletes it"
            ));
        }
    }

    // Saves are serialized so two clients cannot interleave their
    // check-then-write sequences and silently overwrite each other. The lock
    // covers the profile actions too: a create must be visible to this save's
    // own credential and config steps, and a concurrent save must not resolve
    // a profile this one is in the middle of deleting.
    static HERMES_PERSIST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _persist_guard = HERMES_PERSIST_LOCK.lock().await;

    // Profile directories first, so the rest of this save sees the result.
    // A failure here aborts the whole save rather than degrading to a partial
    // outcome: the later steps are written against profiles that were supposed
    // to exist (or not) and would otherwise act on the wrong set.
    let home = hermes_config::hermes_home_dir()?;
    for action in &doc.profile_actions {
        match action {
            HermesProfileAction::CreateProfile {
                name,
                copy_config_from,
            } => {
                hermes_config::create_profile_in(&home, name, copy_config_from.as_deref())?;
            }
            HermesProfileAction::DeleteProfile { name } => {
                hermes_config::delete_profile_in(&home, name)?;
            }
        }
    }

    // Group credential actions per profile so each profile pays for one
    // gateway spawn, and run them before config writes so a newly keyed
    // provider is usable by the saved config.
    let mut actions_by_profile: Vec<(HermesProfileRef, Vec<HermesCredentialAction>)> = Vec::new();
    for action in &doc.actions {
        let profile_name = match action {
            HermesCredentialAction::SaveApiKey { profile, .. }
            | HermesCredentialAction::Disconnect { profile, .. } => profile.as_str(),
        };
        let profile = hermes_config::resolve_profile_ref(Some(profile_name))?;
        match actions_by_profile
            .iter_mut()
            .find(|(existing, _)| *existing == profile)
        {
            Some((_, actions)) => actions.push(action.clone()),
            None => actions_by_profile.push((profile, vec![action.clone()])),
        }
    }
    // Resolve profiles and conflict-check against the client's base BEFORE
    // any config mutation, so credential actions never run for a save whose
    // config sections would then be refused: a save based on a stale snapshot
    // must not silently overwrite whatever changed the config in the meantime
    // (Hermes CLI, another client).
    let stale_save = |name: &str| {
        format!(
            "Hermes profile '{name}' configuration changed since it was loaded; \
             reload the settings and re-apply your edits"
        )
    };
    let mut config_writes = Vec::new();
    for profile_settings in &doc.profiles {
        if deleted_profiles.contains(&profile_settings.name.as_str()) {
            continue;
        }
        let profile = hermes_config::resolve_profile_ref(Some(&profile_settings.name))?;
        let current = hermes_config::load_profile_config(&profile.home_dir)?;
        if current == profile_settings.config {
            continue;
        }
        // A changed section without its base is an unverifiable save — refuse
        // it rather than fall back to last-writer-wins.
        let Some(base) = &profile_settings.base_config else {
            return Err(format!(
                "Hermes profile '{}' settings update is missing its base configuration; \
                 reload the settings and try again",
                profile_settings.name
            ));
        };
        if current != *base {
            return Err(stale_save(&profile_settings.name));
        }
        config_writes.push((profile, base.clone(), profile_settings.config.clone()));
    }

    let mut credential_errors = Vec::new();
    for (profile, actions) in &actions_by_profile {
        if let Err(error) =
            run_credential_actions_for_profile(workspace_roots, profile, actions).await
        {
            credential_errors.push(error);
        }
    }

    for (profile, base, config) in &config_writes {
        // Credential actions awaited above; re-verify the base right before
        // writing so an external edit in that window is refused, not
        // overwritten.
        let current = hermes_config::load_profile_config(&profile.home_dir)?;
        if current != *base && current != *config {
            return Err(stale_save(&profile.name));
        }
        hermes_config::apply_profile_config(&profile.home_dir, config)?;
    }
    if credential_errors.is_empty() {
        Ok(HermesNativeSettingsSaveOutcome::Complete)
    } else {
        Ok(HermesNativeSettingsSaveOutcome::Partial { credential_errors })
    }
}

async fn run_credential_actions_for_profile(
    workspace_roots: &[String],
    profile: &HermesProfileRef,
    actions: &[protocol::hermes_config::HermesCredentialAction],
) -> Result<(), String> {
    use protocol::hermes_config::HermesCredentialAction;

    let mut errors = Vec::new();
    let mut gateway_actions = Vec::new();
    for action in actions {
        match action {
            HermesCredentialAction::Disconnect { provider, .. } if !profile.is_default() => {
                errors.push(format!(
                    "disconnect for provider '{provider}' is disabled because Hermes cannot \
                     prove that credential deletion is scoped to named profile '{}'",
                    profile.name
                ));
            }
            _ => gateway_actions.push(action),
        }
    }
    if gateway_actions.is_empty() {
        return Err(format!("profile '{}': {}", profile.name, errors.join("; ")));
    }

    let (gateway, _events) = HermesGatewayHandle::spawn(
        workspace_roots,
        &[],
        &protocol::ToolPolicy::Unrestricted,
        profile,
        &ResolvedSpawnConfig::default(),
        false,
    )
    .await?;
    for action in gateway_actions {
        let outcome = match action {
            HermesCredentialAction::SaveApiKey {
                provider, api_key, ..
            } => {
                if api_key.trim().is_empty() {
                    Err(format!("no API key provided for provider '{provider}'"))
                } else {
                    gateway
                        .request(
                            "model.save_key",
                            json!({ "slug": provider, "api_key": api_key }),
                        )
                        .await
                        .map(|_| ())
                }
            }
            HermesCredentialAction::Disconnect { provider, .. } => gateway
                .request("model.disconnect", json!({ "slug": provider }))
                .await
                .map(|_| ()),
        };
        if let Err(error) = outcome {
            errors.push(error);
        }
    }
    gateway.shutdown().await;
    if errors.is_empty() {
        Ok(())
    } else {
        Err(format!("profile '{}': {}", profile.name, errors.join("; ")))
    }
}

/// One discovered Hermes profile as the launch-profile catalog sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HermesLaunchProfileInfo {
    pub name: String,
    /// `provider/model` summary of the profile's current selection, when its
    /// gateway probe succeeded and reported one.
    pub summary: Option<String>,
    /// Why this profile is not selectable (its gateway probe failed).
    pub error: Option<String>,
}

/// Result of the Hermes session-schema probe: the schema itself plus the
/// discovered profiles that back its `profile` field, for launch-profile
/// catalog synthesis.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct HermesSessionSchemaProbe {
    pub schema: SessionSettingsSchema,
    pub profiles: Vec<HermesLaunchProfileInfo>,
}

pub(crate) async fn probe_session_settings_schema(
    workspace_roots: &[String],
    disabled_providers: &HashMap<String, Vec<String>>,
) -> Result<HermesSessionSchemaProbe, String> {
    let profiles = hermes_config::discover_profiles()?;
    let probes = futures_util::future::join_all(
        profiles
            .iter()
            .map(|profile| probe_model_options(workspace_roots, profile)),
    )
    .await;
    session_schema_probe_from_model_options(&profiles, probes, disabled_providers)
}

/// Assemble the session schema from per-profile `model.options` payloads.
/// The default profile's payload must be usable (matching the pre-profile
/// behavior of failing the schema when Hermes has no authenticated models);
/// a broken named profile is reported per-profile instead of hiding the
/// whole backend.
fn session_schema_probe_from_model_options(
    profiles: &[HermesProfileRef],
    payloads: Vec<Result<Value, String>>,
    disabled_providers: &HashMap<String, Vec<String>>,
) -> Result<HermesSessionSchemaProbe, String> {
    struct ProfileModels {
        name: String,
        options: Vec<SelectOption>,
        default: Option<String>,
    }

    let mut infos = Vec::new();
    let mut per_profile = Vec::new();
    for (profile, payload) in profiles.iter().zip(payloads) {
        let disabled = disabled_providers
            .get(&profile.name)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let parsed = payload.and_then(|payload| {
            model_select_options_from_payload(&payload, disabled)
                .map(|(options, default)| (options, default, model_summary_from_payload(&payload)))
        });
        match parsed {
            Ok((options, default, summary)) => {
                infos.push(HermesLaunchProfileInfo {
                    name: profile.name.clone(),
                    summary,
                    error: None,
                });
                per_profile.push(ProfileModels {
                    name: profile.name.clone(),
                    options,
                    default,
                });
            }
            Err(error) => {
                if profile.is_default() {
                    return Err(error);
                }
                infos.push(HermesLaunchProfileInfo {
                    name: profile.name.clone(),
                    summary: None,
                    error: Some(error),
                });
            }
        }
    }

    let default_profile = per_profile
        .iter()
        .find(|models| models.name == hermes_config_default_profile())
        .ok_or_else(|| "Hermes default profile probe produced no models".to_string())?;

    let mut fields = Vec::new();
    if infos.len() > 1 {
        fields.push(SessionSettingField {
            key: HERMES_PROFILE_SETTING.to_string(),
            label: "Profile".to_string(),
            description: Some(
                "Hermes profile (an independent HERMES_HOME with its own \
                 configuration and credentials) backing this session."
                    .to_string(),
            ),
            use_slider: false,
            select_options_by_setting: None,
            field_type: SessionSettingFieldType::Select {
                options: infos
                    .iter()
                    .map(|info| SelectOption {
                        value: info.name.clone(),
                        label: if info.name == hermes_config_default_profile() {
                            "Default".to_string()
                        } else if let Some(error) = &info.error {
                            format!("{} — Unavailable: {error}", info.name)
                        } else {
                            info.name.clone()
                        },
                    })
                    .collect(),
                default: Some(hermes_config_default_profile().to_string()),
                nullable: false,
            },
        });
    }
    fields.push(SessionSettingField {
        key: "model".to_string(),
        label: "Model".to_string(),
        description: Some(
            "Hermes model from authenticated providers reported by model.options.".to_string(),
        ),
        use_slider: false,
        select_options_by_setting: (infos.len() > 1).then(|| protocol::SelectOptionsBySetting {
            setting_key: HERMES_PROFILE_SETTING.to_string(),
            values: per_profile
                .iter()
                .map(|models| protocol::SelectOptionsForValue {
                    setting_value: models.name.clone(),
                    options: models.options.clone(),
                })
                .collect(),
        }),
        field_type: SessionSettingFieldType::Select {
            options: default_profile.options.clone(),
            default: default_profile.default.clone(),
            nullable: true,
        },
    });
    fields.extend(hermes_base_session_fields());

    Ok(HermesSessionSchemaProbe {
        schema: SessionSettingsSchema {
            backend_kind: BackendKind::Hermes,
            fields,
        },
        profiles: infos,
    })
}

fn hermes_config_default_profile() -> &'static str {
    protocol::hermes_config::HERMES_DEFAULT_PROFILE
}

/// The profile's currently effective `provider/model` from a `model.options`
/// payload, for display.
fn model_summary_from_payload(value: &Value) -> Option<String> {
    let model = optional_string(value, &["model"]).filter(|model| !model.trim().is_empty())?;
    match optional_string(value, &["provider"]).filter(|provider| !provider.trim().is_empty()) {
        Some(provider) => Some(format!("{provider}/{model}")),
        None => Some(model),
    }
}

impl HermesSessionActor {
    async fn handle_compaction(
        &mut self,
        request: BackendCompactionRequest,
    ) -> BackendCompactionStart {
        // Transcript authority is the first guard by contract: an advertised
        // native method must not bypass an unsafe replay source.
        if !request.transcript_authoritative {
            return BackendCompactionStart::NotDispatched {
                reason: BackendCompactionNotDispatchedReason::NativeUnavailable(
                    BackendCompactionUnavailableReason::TranscriptNotAuthoritative,
                ),
                fallback_safe: true,
            };
        }
        if self
            .active_compaction
            .lock()
            .expect("Hermes active compaction mutex poisoned")
            .is_some()
        {
            return BackendCompactionStart::Deferred {
                reason: BackendCompactionDeferredReason::AnotherCompactionActive,
            };
        }
        if self.mapper.current_message_id.is_some()
            || self.mapper.current_reasoning_seen
            || !self.mapper.pending_tools.is_empty()
            || self.mapper.pending_approval_tool_id.is_some()
            || self.mapper.awaiting_interrupted_complete
        {
            return BackendCompactionStart::Deferred {
                reason: BackendCompactionDeferredReason::ActiveTurn,
            };
        }
        if !self.mapper.background_tasks.is_empty() || !self.native_subagents.is_empty() {
            return BackendCompactionStart::Deferred {
                reason: BackendCompactionDeferredReason::BackgroundMutationActive,
            };
        }

        let (terminal_tx, terminal) = oneshot::channel();
        let operation_id = request.operation_id.clone();
        let live_session_id = self.live_session_id.clone();
        let stored_before = self
            .stored_session_id
            .lock()
            .expect("Hermes stored session id mutex poisoned")
            .clone();
        let gateway = self.gateway.clone();
        let focus = request.focus.clone();
        self.emit_compaction_progress(&operation_id, CompactionStage::Dispatching);
        let mut params = json!({ "session_id": live_session_id.clone() });
        if let Some(focus) = focus.filter(|focus| !focus.trim().is_empty()) {
            params["focus_topic"] = Value::String(focus);
        }
        let response = match gateway.dispatch_request("session.compress", params).await {
            Ok(response) => response,
            Err(HermesDispatchError::NotSent) => {
                return BackendCompactionStart::NotDispatched {
                    reason: BackendCompactionNotDispatchedReason::BackendClosed,
                    fallback_safe: false,
                };
            }
            Err(HermesDispatchError::Uncertain(error)) => {
                let result = hermes_dispatch_uncertain_result(
                    operation_id,
                    live_session_id,
                    stored_before,
                    error,
                );
                return BackendCompactionStart::DispatchUncertain(Box::new(result));
            }
        };
        *self
            .active_compaction
            .lock()
            .expect("Hermes active compaction mutex poisoned") =
            Some((operation_id.clone(), std::time::Instant::now()));
        let stored_session_id = Arc::clone(&self.stored_session_id);
        let compaction_capability = Arc::clone(&self.compaction_capability);
        let active_compaction = Arc::clone(&self.active_compaction);
        let events_tx = self.events_tx.clone();
        let terminal_operation_id = operation_id.clone();
        tokio::spawn(async move {
            let response = response.await.unwrap_or_else(|_| {
                Err(HermesRpcError {
                    code: None,
                    message: "Hermes response channel closed during session.compress".to_string(),
                })
            });
            let _ = events_tx.send(BackendEvent::Compaction(BackendCompactionEvent::Progress(
                BackendCompactionProgress {
                    operation_id: terminal_operation_id.clone(),
                    stage: CompactionStage::Finalizing,
                    elapsed_ms: None,
                },
            )));
            let result = classify_hermes_compaction_response(
                terminal_operation_id,
                live_session_id,
                stored_before,
                response,
                &stored_session_id,
                &compaction_capability,
            );
            *active_compaction
                .lock()
                .expect("Hermes active compaction mutex poisoned") = None;
            let _ = terminal_tx.send(result);
        });
        BackendCompactionStart::Accepted(BackendAcceptedCompaction {
            operation_id,
            terminal,
        })
    }

    fn emit_compaction_progress(
        &self,
        operation_id: &protocol::CompactionOperationId,
        stage: CompactionStage,
    ) {
        let _ = self
            .events_tx
            .send(BackendEvent::Compaction(BackendCompactionEvent::Progress(
                BackendCompactionProgress {
                    operation_id: operation_id.clone(),
                    stage,
                    elapsed_ms: None,
                },
            )));
    }

    async fn run(
        mut self,
        initial_input: Option<protocol::SendMessagePayload>,
        replay: Option<(Vec<ChatEvent>, oneshot::Sender<()>)>,
        startup_gateway_events: Vec<HermesGatewayEvent>,
    ) {
        if let Some((events, barrier)) = replay {
            for event in events {
                if self.events_tx.send(BackendEvent::Chat(event)).is_err() {
                    let _ = barrier.send(());
                    self.gateway.shutdown().await;
                    return;
                }
            }
            let _ = barrier.send(());
        }

        for event in startup_gateway_events {
            if !self.handle_gateway_event(event).await {
                self.drain_background_tasks();
                self.gateway.shutdown().await;
                return;
            }
        }

        if let Some(input) = initial_input {
            self.handle_send_message(input).await;
        }

        loop {
            tokio::select! {
                maybe_event = self.gateway_events_rx.recv() => {
                    let Some(event) = maybe_event else {
                        self.emit_error("Hermes gateway event channel closed");
                        break;
                    };
                    if !self.handle_gateway_event(event).await {
                        break;
                    }
                }
                maybe_command = self.command_rx.recv() => {
                    let Some(command) = maybe_command else { break; };
                    match command {
                        HermesBackendCommand::Input(input) => self.handle_input(input).await,
                        HermesBackendCommand::UpdateSessionSettings(payload, reply) => {
                            let result = self.handle_settings_update(payload.values).await;
                            let _ = reply.send(result);
                        }
                        HermesBackendCommand::SetSubagentEmitter(emitter, reply) => {
                            self.subagent_emitter = Some(emitter);
                            let _ = reply.send(());
                        }
                        HermesBackendCommand::Interrupt(reply) => {
                            let ok = self.handle_interrupt().await;
                            let _ = reply.send(ok);
                        }
                        HermesBackendCommand::Compact(request, reply) => {
                            let start = self.handle_compaction(request).await;
                            let _ = reply.send(start);
                        }
                        HermesBackendCommand::Shutdown(reply) => {
                            self.drain_background_tasks();
                            self.gateway.shutdown().await;
                            let _ = reply.send(());
                            return;
                        }
                    }
                }
            }
        }

        self.drain_background_tasks();
        self.gateway.shutdown().await;
    }

    fn drain_background_tasks(&mut self) {
        for event in self.mapper.drain_background_tasks() {
            self.emit(event);
        }
    }

    async fn handle_input(&mut self, input: AgentInput) {
        match input {
            AgentInput::SendMessage(payload) => {
                if let Some(response) = payload.tool_response.clone() {
                    self.handle_tool_response(response, payload.message).await;
                } else {
                    self.handle_send_message(payload).await;
                }
            }
            AgentInput::UpdateSessionSettings(payload) => {
                if let Err(error) = self.handle_settings_update(payload.values).await {
                    self.emit_error(error);
                }
            }
            AgentInput::EditQueuedMessage(_)
            | AgentInput::CancelQueuedMessage(_)
            | AgentInput::SendQueuedMessageNow(_) => {
                self.emit_error("queued-message inputs reached Hermes backend");
            }
        }
    }

    async fn handle_send_message(&mut self, payload: protocol::SendMessagePayload) {
        if payload
            .images
            .as_ref()
            .is_some_and(|images| !images.is_empty())
        {
            self.emit_error(
                "Hermes image input is disabled until the native gateway contract is verified",
            );
            return;
        }
        if payload.message.trim().is_empty() {
            self.emit_error("Hermes prompt.submit requires a non-empty message");
            return;
        }

        // Scope the stderr tail to this turn so a later failure never reports
        // stale output from an earlier one.
        self.recent_stderr.clear();
        self.emit(ChatEvent::MessageAdded(user_message(&payload.message)));
        self.emit(ChatEvent::TypingStatusChanged(true));
        match self
            .gateway
            .request(
                "prompt.submit",
                json!({
                    "session_id": self.live_session_id,
                    "text": payload.message,
                }),
            )
            .await
        {
            Ok(result) => match required_string(&result, &["status"], "prompt.submit") {
                Ok(status) if status == "streaming" || status == "queued" => {}
                Ok(status) => self.emit_turn_failure(format!(
                    "Hermes prompt.submit returned unexpected status '{status}'"
                )),
                Err(err) => self.emit_turn_failure(err),
            },
            Err(err) => {
                self.emit_turn_failure(format!("Hermes prompt.submit failed: {err}"));
            }
        }
    }

    async fn handle_tool_response(&mut self, response: SendMessageToolResponse, message: String) {
        match response {
            SendMessageToolResponse::ExitPlanMode {
                tool_call_id,
                decision,
                feedback: _,
            } => {
                let Some(pending) = self.mapper.pending_approval_tool_id.clone() else {
                    self.emit_error("Hermes received approval response with no pending approval");
                    return;
                };
                if pending != tool_call_id {
                    self.emit_error(format!(
                        "Hermes approval response tool_call_id mismatch: expected {pending}, got {tool_call_id}"
                    ));
                    return;
                }
                let choice = match decision {
                    protocol::ExitPlanModeDecision::Approve => "allow",
                    protocol::ExitPlanModeDecision::Reject => "deny",
                };
                match self
                    .gateway
                    .request(
                        "approval.respond",
                        json!({
                            "session_id": self.live_session_id,
                            "choice": choice,
                            "message": message,
                        }),
                    )
                    .await
                {
                    Ok(result) => {
                        self.mapper.pending_approval_tool_id = None;
                        self.mapper.pending_tools.remove(&tool_call_id);
                        self.mapper.pending_tool_arguments.remove(&tool_call_id);
                        self.emit(ChatEvent::ToolExecutionCompleted(
                            ToolExecutionCompletedData {
                                tool_call_id,
                                tool_name: "approval.request".to_string(),
                                tool_result: ToolExecutionResult::Other { result },
                                success: true,
                                error: None,
                                normalization_failure: None,
                            },
                        ));
                        self.emit(ChatEvent::TypingStatusChanged(true));
                    }
                    Err(err) => self.emit_error(format!("Hermes approval.respond failed: {err}")),
                }
            }
        }
    }

    async fn handle_settings_update(
        &mut self,
        values: SessionSettingsValues,
    ) -> Result<(), String> {
        for (key, value) in values.0 {
            match (key.as_str(), value) {
                ("model", SessionSettingValue::String(model)) if !model.trim().is_empty() => {
                    let Some(selection) = parse_hermes_model_setting(&model) else {
                        return Err(format!("invalid Hermes model setting '{model}'"));
                    };
                    let switch_value =
                        hermes_model_switch_value(&selection.model, selection.provider.as_deref());
                    match self
                        .gateway
                        .request(
                            "config.set",
                            json!({
                                "session_id": self.live_session_id,
                                "key": "model",
                                "value": switch_value,
                            }),
                        )
                        .await
                    {
                        Ok(result) => {
                            self.mapper.model =
                                optional_string(&result, &["value"]).or(Some(selection.model));
                            if let Some(provider) = selection.provider {
                                self.mapper.provider = Some(provider);
                            }
                            self.refresh_provider_info().await;
                        }
                        Err(err) => return Err(format!("Hermes config.set model failed: {err}")),
                    }
                }
                ("model", SessionSettingValue::Null) => {}
                ("reasoning_effort", SessionSettingValue::String(effort))
                    if !effort.trim().is_empty() =>
                {
                    if let Err(err) = self
                        .gateway
                        .request(
                            "config.set",
                            json!({
                                "session_id": self.live_session_id,
                                "key": "reasoning",
                                "value": effort,
                            }),
                        )
                        .await
                    {
                        return Err(format!("Hermes config.set reasoning failed: {err}"));
                    }
                }
                ("reasoning_effort", SessionSettingValue::Null) => {}
                ("fast", SessionSettingValue::Bool(fast)) => {
                    let value = if fast { "fast" } else { "normal" };
                    if let Err(err) = self
                        .gateway
                        .request(
                            "config.set",
                            json!({
                                "session_id": self.live_session_id,
                                "key": "fast",
                                "value": value,
                            }),
                        )
                        .await
                    {
                        return Err(format!("Hermes config.set fast failed: {err}"));
                    }
                }
                (unknown, _) => {
                    return Err(format!("unsupported Hermes session setting '{unknown}'"));
                }
            }
        }
        Ok(())
    }

    async fn refresh_provider_info(&mut self) {
        match self
            .gateway
            .request("config.get", json!({ "key": "provider" }))
            .await
        {
            Ok(result) => {
                self.mapper.model =
                    optional_string(&result, &["model"]).or(self.mapper.model.take());
                self.mapper.provider = optional_string(&result, &["provider"]);
            }
            Err(err) => self.emit_error(format!("Hermes config.get provider failed: {err}")),
        }
    }

    async fn handle_interrupt(&mut self) -> bool {
        match self
            .gateway
            .request(
                "session.interrupt",
                json!({ "session_id": self.live_session_id }),
            )
            .await
        {
            Ok(_) => {
                let events = self.mapper.cancel_events("Operation cancelled");
                for event in events {
                    self.emit(event);
                }
                true
            }
            Err(err) => {
                self.emit_error(format!("Hermes session.interrupt failed: {err}"));
                false
            }
        }
    }

    async fn handle_gateway_event(&mut self, event: HermesGatewayEvent) -> bool {
        match event {
            HermesGatewayEvent::Event {
                event_type,
                session_id,
                mut payload,
            } => {
                if !event_targets_session(session_id.as_deref(), &self.live_session_id) {
                    return true;
                }
                if event_type == "status"
                    && payload
                        .as_ref()
                        .and_then(|value| optional_string(value, &["status", "state"]))
                        .as_deref()
                        == Some("compressing")
                {
                    let active = self
                        .active_compaction
                        .lock()
                        .expect("Hermes active compaction mutex poisoned")
                        .clone();
                    if let Some((operation_id, started_at)) = active {
                        let _ = self.events_tx.send(BackendEvent::Compaction(
                            BackendCompactionEvent::Progress(BackendCompactionProgress {
                                operation_id,
                                stage: CompactionStage::Compacting,
                                elapsed_ms: Some(started_at.elapsed().as_millis() as u64),
                            }),
                        ));
                        return true;
                    }
                }
                tracing::debug!(
                    event_type,
                    stream_open = self.mapper.current_message_id.is_some(),
                    pending_tools = self.mapper.pending_tools.len(),
                    "mapping Hermes gateway event"
                );
                if event_type.starts_with("subagent.") {
                    self.handle_native_subagent_event(&event_type, payload)
                        .await;
                    return true;
                }
                if event_type == "message.complete" {
                    let status = payload
                        .as_ref()
                        .and_then(|value| optional_string(value, &["status"]));
                    let cancelled_settlement = (self.mapper.awaiting_interrupted_complete
                        && self.mapper.current_message_id.is_none())
                        || status.as_deref() == Some("interrupted");
                    if !cancelled_settlement {
                        payload = self.enrich_message_complete_payload(payload).await;
                    }
                }
                let mapped = self.mapper.map_event(&event_type, payload);
                for event in mapped {
                    self.emit(event);
                }
                true
            }
            HermesGatewayEvent::ProtocolError(message) => {
                if self
                    .active_compaction
                    .lock()
                    .expect("Hermes active compaction mutex poisoned")
                    .is_none()
                {
                    self.emit_turn_failure(format!("Hermes gateway protocol error: {message}"));
                }
                true
            }
            HermesGatewayEvent::Stderr(line) => {
                // Hermes stderr is diagnostic decoration — retry panels ("API
                // call failed (attempt 1/3)", provider/endpoint/elapsed banners),
                // capability notes, and the like — not a user-facing error
                // channel. Surfacing one chat warning per line fragments a single
                // failure across many cards and cries wolf on transient retries
                // that ultimately succeed. Genuine turn failures arrive as
                // protocol "error"/"failed" events (see map_error) and gateway
                // death via Closed, each of which surfaces one coherent message.
                // Keep the raw stderr in the host log for debugging, and retain a
                // bounded tail so a message-less failure (gateway exit) can still
                // report the real cause.
                tracing::debug!(message = %line, "Hermes stderr");
                if self.recent_stderr.len() == HERMES_STDERR_TAIL {
                    self.recent_stderr.pop_front();
                }
                self.recent_stderr.push_back(line);
                true
            }
            HermesGatewayEvent::Closed(exit_code) => {
                let base = match exit_code {
                    Some(code) => format!("Hermes gateway exited with code {code}"),
                    None => "Hermes gateway exited".to_string(),
                };
                let message = format_failure_with_stderr_tail(base, &self.recent_stderr);
                if self
                    .active_compaction
                    .lock()
                    .expect("Hermes active compaction mutex poisoned")
                    .is_none()
                {
                    self.emit_turn_failure(message);
                }
                false
            }
        }
    }

    async fn handle_native_subagent_event(&mut self, event_type: &str, payload: Option<Value>) {
        let Some(payload) = payload else {
            self.emit_error(format!("Hermes {event_type} omitted its payload"));
            return;
        };
        let subagent_id = match optional_string_any(&payload, &["subagent_id", "child_session_id"])
        {
            Some(id) => id,
            None => {
                let task_index = payload
                    .get("task_index")
                    .and_then(Value::as_u64)
                    .unwrap_or_default();
                let live = &self.native_subagents;
                resolve_synthetic_subagent_id(
                    &format!("hermes-subagent-{task_index}"),
                    |id| live.contains_key(id),
                    &mut self.synthetic_subagent_ids,
                )
            }
        };
        let description = optional_string_any(&payload, &["goal", "text"]).unwrap_or_default();

        if !self.native_subagents.contains_key(&subagent_id) {
            let Some(emitter) = self.subagent_emitter.as_ref().cloned() else {
                self.emit_error(format!(
                    "Hermes {event_type} arrived before the native sub-agent emitter was installed"
                ));
                return;
            };
            let parent_anchor = self
                .mapper
                .resolve_delegation_anchor(&payload, &description);
            if parent_anchor.is_none() {
                tracing::debug!(
                    subagent_id = %subagent_id,
                    event_type,
                    "Hermes native child has no unambiguous delegation card; its progress stays unanchored"
                );
            }
            let task_index = payload
                .get("task_index")
                .and_then(Value::as_u64)
                .unwrap_or_default();
            let agent_name = optional_string(&payload, &["name"])
                .unwrap_or_else(|| format!("Hermes Agent {}", task_index + 1));
            let session_id_hint = optional_string(&payload, &["child_session_id"]).map(SessionId);
            let handle = match emitter
                .on_subagent_spawned(
                    subagent_id.clone(),
                    agent_name.clone(),
                    description.clone(),
                    "hermes_native".to_string(),
                    session_id_hint,
                )
                .await
            {
                Ok(handle) => handle,
                Err(error) => {
                    self.emit_error(format!(
                        "Hermes native child registration failed for {subagent_id}: {error}"
                    ));
                    return;
                }
            };
            self.native_subagents.insert(
                subagent_id.clone(),
                HermesNativeSubagent {
                    handle,
                    agent_name,
                    parent_anchor,
                    tool_calls: 0,
                },
            );
        }

        let mut child_event = None;
        let mut parent_progress = None;
        if let Some(child) = self.native_subagents.get_mut(&subagent_id) {
            if event_type == "subagent.tool" {
                child.tool_calls = child.tool_calls.saturating_add(1);
            }
            let completed = event_type == "subagent.complete";
            parent_progress = child.parent_anchor.as_ref().map(|anchor| {
                hermes_subagent_progress(
                    &child.handle,
                    &child.agent_name,
                    anchor,
                    child.tool_calls,
                    completed,
                )
            });
            if completed {
                let content = optional_string_any(&payload, &["summary", "text"])
                    .unwrap_or_else(|| "Hermes child completed.".to_string());
                child_event = Some(ChatEvent::MessageAdded(ChatMessage {
                    message_id: None,
                    timestamp: unix_now_ms(),
                    sender: MessageSender::Assistant {
                        agent: HERMES_AGENT_NAME.to_string(),
                    },
                    content,
                    reasoning: None,
                    tool_calls: Vec::new(),
                    model_info: optional_string(&payload, &["model"])
                        .map(|model| ModelInfo { model }),
                    token_usage: None,
                    context_breakdown: None,
                    images: None,
                }));
            }
        }
        if let Some(progress) = parent_progress {
            self.emit(ChatEvent::ToolProgress(progress));
        }
        if let Some(event) = child_event
            && let Some(child) = self.native_subagents.get(&subagent_id)
        {
            let _ = child.handle.event_tx.send(event);
        }
        if event_type == "subagent.complete" {
            self.native_subagents.remove(&subagent_id);
        }
    }

    async fn enrich_message_complete_payload(&mut self, payload: Option<Value>) -> Option<Value> {
        let mut payload = payload?;
        let mut turn_usage = payload.get("usage").and_then(token_usage_from_value);
        if turn_usage.is_none() {
            let usage_result = tokio::time::timeout(
                HERMES_USAGE_TIMEOUT,
                self.gateway.request(
                    "session.usage",
                    json!({ "session_id": self.live_session_id }),
                ),
            )
            .await;
            turn_usage = match usage_result {
                Ok(Ok(value)) => token_usage_from_value(&value),
                Ok(Err(err)) => {
                    tracing::debug!(error = %err, "Hermes session.usage failed");
                    None
                }
                Err(_) => {
                    tracing::debug!("Hermes session.usage timed out");
                    None
                }
            };
            if turn_usage.is_none() {
                tracing::debug!("Hermes session.usage did not report token counts");
            }
        }

        let context_result = tokio::time::timeout(
            HERMES_USAGE_TIMEOUT,
            self.gateway.request(
                "session.context_breakdown",
                json!({ "session_id": self.live_session_id }),
            ),
        )
        .await;
        let context_breakdown = match context_result {
            Ok(Ok(value)) => match context_breakdown_from_hermes(&value) {
                Some(context_breakdown) => Some(context_breakdown),
                None => {
                    tracing::debug!(
                        "Hermes session.context_breakdown did not report context usage"
                    );
                    None
                }
            },
            Ok(Err(err)) => {
                // Leaner/older Hermes gateways don't implement this optional
                // method and answer JSON-RPC -32601. That's a capability gap, not
                // a failure the user needs to see on every otherwise-working
                // turn — keep it in the log and only warn on genuine breakage.
                if is_unsupported_gateway_method(&err) {
                    tracing::debug!(error = %err, "Hermes gateway lacks session.context_breakdown");
                } else {
                    tracing::debug!(error = %err, "Hermes session.context_breakdown failed");
                }
                None
            }
            Err(_) => {
                tracing::debug!("Hermes session.context_breakdown timed out");
                None
            }
        };

        if let Some(object) = payload.as_object_mut() {
            if let Some(session_usage) = turn_usage {
                let (turn_usage, cumulative_usage) =
                    self.mapper.record_session_usage(session_usage);
                object.insert(
                    "usage".to_string(),
                    token_usage_to_gateway_value(&turn_usage),
                );
                if let Some(cumulative_usage) = cumulative_usage {
                    object.insert(
                        "cumulative_usage".to_string(),
                        token_usage_to_gateway_value(&cumulative_usage),
                    );
                } else {
                    object.remove("cumulative_usage");
                }
            }
            if let Some(context_breakdown) = context_breakdown {
                object.insert(
                    "context_breakdown".to_string(),
                    serde_json::to_value(context_breakdown)
                        .expect("Hermes context breakdown must serialize"),
                );
            }
        }
        Some(payload)
    }

    fn emit(&self, event: ChatEvent) {
        let _ = self.events_tx.send(BackendEvent::Chat(event));
    }

    fn emit_error(&self, message: impl Into<String>) {
        let _ = self
            .events_tx
            .send(BackendEvent::Chat(ChatEvent::MessageAdded(error_message(
                message.into(),
            ))));
    }

    fn emit_turn_failure(&mut self, message: impl Into<String>) {
        for event in self.mapper.fail_active_turn(message.into()) {
            self.emit(event);
        }
    }
}

/// Append the buffered stderr tail to a failure message that would otherwise
/// carry no detail, so a gateway that died mid-call after exhausting API retries
/// still reports why. No-op when there is nothing buffered.
fn format_failure_with_stderr_tail(base: String, recent_stderr: &VecDeque<String>) -> String {
    if recent_stderr.is_empty() {
        return base;
    }
    let tail = recent_stderr
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join("\n");
    format!("{base}\n\nRecent Hermes output:\n{tail}")
}

/// True when a gateway request failed because this Hermes build doesn't
/// implement the method (JSON-RPC -32601). Tyde calls a few optional methods
/// (e.g. `session.context_breakdown`) that leaner/older gateways lack; those
/// belong in the log, not in a user-facing warning on every working turn.
fn is_unsupported_gateway_method(error: &str) -> bool {
    error.contains("-32601") || error.contains("unknown method")
}

impl HermesGatewayHandle {
    async fn spawn(
        workspace_roots: &[String],
        startup_mcp_servers: &[StartupMcpServer],
        tool_policy: &protocol::ToolPolicy,
        profile: &HermesProfileRef,
        resolved: &ResolvedSpawnConfig,
        expose_skills: bool,
    ) -> Result<(Self, mpsc::UnboundedReceiver<HermesGatewayEvent>), String> {
        let mut target = resolve_gateway_spawn_target(workspace_roots).await?;
        // A named profile is selected by pointing HERMES_HOME at its
        // directory. Remote spawns run over SSH without env forwarding, so a
        // named profile there would silently run against the wrong home —
        // fail visibly instead. The default profile sets nothing and lets
        // Hermes resolve its own home.
        if !profile.is_default() {
            if target.remote_host.is_some() {
                return Err(format!(
                    "Hermes profile '{}' cannot be used with an SSH-backed workspace; \
                     profiles select a local HERMES_HOME directory",
                    profile.name
                ));
            }
            target.env.insert(
                crate::backend::hermes_config::HERMES_HOME_ENV.to_string(),
                profile.home_dir.to_string_lossy().to_string(),
            );
        }
        // Before the gateway starts, and before the instructions are rendered:
        // whether this session may *name* its skills is exactly whether
        // registering the store took. A registration that fails costs the
        // session its skills — never the session itself — and the prompt is then
        // rendered without them, because naming a skill Hermes cannot load is
        // worse than silence. Remote targets never get here: the caller drops
        // their skills, because this edits a config file on *this* machine.
        let registration = if expose_skills && target.remote_host.is_none() {
            Some(register_hermes_skill_dirs(&target, &resolved.skills).await)
        } else {
            None
        };
        let skills_discoverable = registration.as_ref().is_some_and(Result::is_ok);
        let (spawn_instructions, skill_notice) = hermes_skill_exposure(resolved, registration);
        if let Some(notice) = skill_notice.as_deref() {
            tracing::warn!("{notice}");
        }
        let system_overlay_installed = target.remote_host.is_none();
        if system_overlay_installed && let Some(spawn_instructions) = spawn_instructions.as_deref()
        {
            target.env.insert(
                TYDE_HERMES_SYSTEM_PROMPT_ENV.to_string(),
                spawn_instructions.to_string(),
            );
        }
        let mcp_runtime = prepare_hermes_mcp_runtime(
            &mut target,
            startup_mcp_servers,
            tool_policy,
            skills_discoverable,
        )
        .await?;
        let expects_mcp_tools = !startup_mcp_servers.is_empty();
        let mcp_ready_path = mcp_runtime
            .as_ref()
            .map(|runtime| runtime.ready_path.clone());
        let startup_timeout =
            duration_from_env_ms(HERMES_STARTUP_TIMEOUT_ENV, HERMES_STARTUP_TIMEOUT);
        let request_timeout = duration_from_env_ms(HERMES_RPC_TIMEOUT_ENV, HERMES_REQUEST_TIMEOUT);

        let mut child = spawn_gateway_child(&target).await?;
        let stdin = child
            .inner()
            .stdin
            .take()
            .ok_or_else(|| "Failed to capture Hermes gateway stdin".to_string())?;
        let stdout = child
            .inner()
            .stdout
            .take()
            .ok_or_else(|| "Failed to capture Hermes gateway stdout".to_string())?;
        let stderr = child
            .inner()
            .stderr
            .take()
            .ok_or_else(|| "Failed to capture Hermes gateway stderr".to_string())?;

        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let (inbound_tx, inbound_rx) = mpsc::unbounded_channel();
        let (force_shutdown_tx, force_shutdown_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (ready_tx, ready_rx) = oneshot::channel();

        spawn_stdout_reader(stdout, inbound_tx.clone());
        spawn_stderr_reader(stderr, inbound_tx.clone());
        spawn_child_waiter(child, inbound_tx, force_shutdown_rx);
        tokio::spawn(run_gateway_actor(
            stdin,
            command_rx,
            inbound_rx,
            event_tx,
            Some(ready_tx),
            mcp_runtime,
            force_shutdown_tx,
        ));

        let handle = Self {
            tx: command_tx,
            request_timeout,
            system_overlay_installed,
            provider_version: target.provider_version.clone(),
            spawn_instructions,
            skill_notice,
        };

        match tokio::time::timeout(startup_timeout, ready_rx).await {
            Ok(Ok(Ok(()))) => {
                if let Some(ready_path) = mcp_ready_path {
                    if let Err(error) =
                        wait_for_hermes_mcp_bridge_ready(&ready_path, startup_timeout).await
                    {
                        handle.shutdown().await;
                        return Err(error);
                    }
                    if expects_mcp_tools
                        && let Err(error) =
                            wait_for_hermes_mcp_tools(&handle, startup_timeout).await
                    {
                        handle.shutdown().await;
                        return Err(error);
                    }
                }
                Ok((handle, event_rx))
            }
            Ok(Ok(Err(err))) => {
                handle.shutdown().await;
                Err(err)
            }
            Ok(Err(_)) => {
                handle.shutdown().await;
                Err("Hermes gateway startup task ended before gateway.ready".to_string())
            }
            Err(_) => {
                handle.shutdown().await;
                Err(format!(
                    "Timed out after {}ms waiting for Hermes gateway.ready from {}",
                    startup_timeout.as_millis(),
                    target.display_program
                ))
            }
        }
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        self.request_typed(method, params)
            .await
            .map_err(|error| error.to_string())
    }

    async fn request_typed(&self, method: &str, params: Value) -> Result<Value, HermesRpcError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(HermesGatewayCommand::Request {
                method: method.to_string(),
                params,
                reply: reply_tx,
                dispatched: None,
            })
            .map_err(|_| HermesRpcError {
                code: None,
                message: format!("Hermes gateway is closed; cannot send {method}"),
            })?;
        match tokio::time::timeout(self.request_timeout, reply_rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(HermesRpcError {
                code: None,
                message: format!("Hermes gateway closed while waiting for {method}"),
            }),
            Err(_) => Err(HermesRpcError {
                code: None,
                message: format!("Hermes request timed out for method '{method}'"),
            }),
        }
    }

    async fn dispatch_request(
        &self,
        method: &str,
        params: Value,
    ) -> Result<oneshot::Receiver<Result<Value, HermesRpcError>>, HermesDispatchError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let (dispatched_tx, dispatched_rx) = oneshot::channel();
        self.tx
            .send(HermesGatewayCommand::Request {
                method: method.to_string(),
                params,
                reply: reply_tx,
                dispatched: Some(dispatched_tx),
            })
            .map_err(|_| HermesDispatchError::NotSent)?;
        match tokio::time::timeout(self.request_timeout, dispatched_rx).await {
            Ok(Ok(Ok(()))) => Ok(reply_rx),
            Ok(Ok(Err(error))) => Err(HermesDispatchError::Uncertain(error)),
            Ok(Err(_)) => Err(HermesDispatchError::Uncertain(HermesRpcError {
                code: None,
                message: format!("Hermes dispatch channel closed for {method}"),
            })),
            Err(_) => Err(HermesDispatchError::Uncertain(HermesRpcError {
                code: None,
                message: format!("Hermes dispatch timed out for method '{method}'"),
            })),
        }
    }

    async fn shutdown(&self) {
        let (reply_tx, reply_rx) = oneshot::channel();
        if self
            .tx
            .send(HermesGatewayCommand::Shutdown(reply_tx))
            .is_ok()
        {
            let _ = tokio::time::timeout(HERMES_SHUTDOWN_TIMEOUT, reply_rx).await;
        }
    }
}

async fn wait_for_hermes_mcp_bridge_ready(path: &Path, timeout: Duration) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        match fs::read(path) {
            Ok(bytes) => {
                let status: Value = serde_json::from_slice(&bytes).map_err(|error| {
                    format!("Hermes MCP bridge published invalid readiness status: {error}")
                })?;
                if status.get("ok").and_then(Value::as_bool) == Some(true) {
                    return Ok(());
                }
                return Err(status
                    .get("error")
                    .and_then(Value::as_str)
                    .map(|error| format!("Hermes MCP bridge failed: {error}"))
                    .unwrap_or_else(|| "Hermes MCP bridge reported startup failure".to_string()));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "Failed to read Hermes MCP bridge readiness status: {error}"
                ));
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!(
                "Timed out after {}ms waiting for Hermes to connect the managed Tyde MCP bridge",
                timeout.as_millis()
            ));
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn wait_for_hermes_mcp_tools(
    gateway: &HermesGatewayHandle,
    timeout: Duration,
) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut last_toolsets: Option<String> = None;
    let mut last_shown = None;
    let mut tools_list_unsupported = false;
    loop {
        if tokio::time::Instant::now() >= deadline {
            return Err(format!(
                "Hermes did not register the managed Tyde MCP toolset{}{}",
                last_toolsets
                    .map(|summary| format!("; last tools.list toolsets: {summary}"))
                    .unwrap_or_default(),
                last_shown
                    .map(|value: Value| format!("; last tools.show result: {value}"))
                    .unwrap_or_default()
            ));
        }
        // The toolset registry (tools.list) is authoritative: it reports the
        // managed toolset even when Hermes's Tool Search feature defers MCP
        // tools behind tool_search/tool_describe/tool_call and hides them
        // from the model-visible tools.show sections. Both probes are bounded
        // by the gate deadline so a slow RPC cannot stretch one poll past it.
        if !tools_list_unsupported {
            match tokio::time::timeout_at(deadline, gateway.request("tools.list", json!({}))).await
            {
                Ok(Ok(result)) => {
                    let toolsets = result.get("toolsets").and_then(Value::as_array);
                    if toolsets
                        .is_some_and(|toolsets| toolsets.iter().any(managed_mcp_toolset_entry))
                    {
                        return Ok(());
                    }
                    last_toolsets = Some(summarize_hermes_toolsets(toolsets));
                }
                Ok(Err(error)) => {
                    if is_unsupported_gateway_method(&error) {
                        tools_list_unsupported = true;
                    } else {
                        last_toolsets = Some(format!("request failed: {error}"));
                    }
                }
                Err(_) => continue,
            }
        }
        // Fallback for gateways without tools.list: the model-visible
        // sections still name the toolset when deferral is inactive.
        match tokio::time::timeout_at(deadline, gateway.request("tools.show", json!({}))).await {
            Ok(Ok(result)) => {
                let registered = result
                    .get("sections")
                    .and_then(Value::as_array)
                    .is_some_and(|sections| {
                        sections.iter().any(|section| {
                            section.get("name").and_then(Value::as_str)
                                == Some(HERMES_MANAGED_MCP_TOOLSET)
                                && section
                                    .get("tools")
                                    .and_then(Value::as_array)
                                    .is_some_and(|tools| !tools.is_empty())
                        })
                    });
                if registered {
                    return Ok(());
                }
                last_shown = Some(result);
            }
            Ok(Err(_)) => {}
            Err(_) => continue,
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// A tools.list entry for the managed Tyde MCP toolset. The gateway displays
/// the toolset under its registered server alias (`tyde`,
/// `MANAGED_SERVER_NAME`) and only falls back to the canonical `mcp-tyde`
/// name when that alias is shadowed. The entry must carry at least one
/// resolved tool and must not be explicitly disabled (a missing `enabled`
/// field counts as enabled, for older gateways that omit it).
fn managed_mcp_toolset_entry(toolset: &Value) -> bool {
    if !matches!(
        toolset.get("name").and_then(Value::as_str),
        Some(MANAGED_SERVER_NAME | HERMES_MANAGED_MCP_TOOLSET)
    ) {
        return false;
    }
    if toolset.get("enabled").and_then(Value::as_bool) == Some(false) {
        return false;
    }
    toolset
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| !tools.is_empty())
}

/// Compact rendering of a tools.list response for the readiness-timeout
/// error: one `name(enabled, tool count)` per toolset, instead of dumping
/// every resolved tool name of every toolset into the error card.
fn summarize_hermes_toolsets(toolsets: Option<&Vec<Value>>) -> String {
    let Some(toolsets) = toolsets else {
        return "missing toolsets array".to_string();
    };
    let entries: Vec<String> = toolsets
        .iter()
        .map(|toolset| {
            format!(
                "{}(enabled={}, tools={})",
                toolset.get("name").and_then(Value::as_str).unwrap_or("?"),
                toolset
                    .get("enabled")
                    .and_then(Value::as_bool)
                    .map(|enabled| enabled.to_string())
                    .unwrap_or_else(|| "?".to_string()),
                toolset
                    .get("tools")
                    .and_then(Value::as_array)
                    .map(Vec::len)
                    .unwrap_or(0),
            )
        })
        .collect();
    format!("[{}]", entries.join(", "))
}

async fn run_gateway_actor(
    stdin: tokio::process::ChildStdin,
    mut command_rx: mpsc::UnboundedReceiver<HermesGatewayCommand>,
    mut inbound_rx: mpsc::UnboundedReceiver<HermesGatewayInbound>,
    event_tx: mpsc::UnboundedSender<HermesGatewayEvent>,
    mut ready_tx: Option<oneshot::Sender<Result<(), String>>>,
    _mcp_runtime: Option<HermesMcpRuntime>,
    force_shutdown_tx: mpsc::UnboundedSender<()>,
) {
    let mut next_id = 1_u64;
    let mut pending: HashMap<u64, oneshot::Sender<Result<Value, HermesRpcError>>> = HashMap::new();
    let mut startup_stderr = VecDeque::new();
    let mut shutdown_reply = None;
    let mut stdin = Some(stdin);

    loop {
        tokio::select! {
            maybe_command = command_rx.recv() => {
                let Some(command) = maybe_command else { break; };
                match command {
                    HermesGatewayCommand::Request { method, params, reply, dispatched } => {
                        if shutdown_reply.is_some() {
                            let error = HermesRpcError {
                                code: None,
                                message: "Hermes gateway is shutting down".to_string(),
                            };
                            if let Some(dispatched) = dispatched {
                                let _ = dispatched.send(Err(error.clone()));
                            }
                            let _ = reply.send(Err(error));
                            continue;
                        }
                        let id = next_id;
                        next_id = next_id.saturating_add(1);
                        let frame = json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "method": method,
                            "params": params,
                        });
                        let line = format!("{}\n", frame);
                        let Some(stdin) = stdin.as_mut() else {
                            let error = HermesRpcError {
                                code: None,
                                message: "Hermes gateway is shutting down".to_string(),
                            };
                            if let Some(dispatched) = dispatched {
                                let _ = dispatched.send(Err(error.clone()));
                            }
                            let _ = reply.send(Err(error));
                            continue;
                        };
                        match stdin.write_all(line.as_bytes()).await {
                            Ok(()) => match stdin.flush().await {
                                Ok(()) => {
                                    pending.insert(id, reply);
                                    if let Some(dispatched) = dispatched {
                                        let _ = dispatched.send(Ok(()));
                                    }
                                }
                                Err(err) => {
                                    let error = HermesRpcError {
                                        code: None,
                                        message: format!("Failed to flush Hermes request {id}: {err}"),
                                    };
                                    if let Some(dispatched) = dispatched {
                                        let _ = dispatched.send(Err(error.clone()));
                                    }
                                    let _ = reply.send(Err(error));
                                }
                            },
                            Err(err) => {
                                let error = HermesRpcError {
                                    code: None,
                                    message: format!("Failed to write Hermes request {id}: {err}"),
                                };
                                if let Some(dispatched) = dispatched {
                                    let _ = dispatched.send(Err(error.clone()));
                                }
                                let _ = reply.send(Err(error));
                            }
                        }
                    }
                    HermesGatewayCommand::Shutdown(reply) => {
                        if shutdown_reply.is_none() {
                            shutdown_reply = Some(reply);
                            // On Unix, ChildStdin's AsyncWrite shutdown does not
                            // close the process pipe; dropping it delivers EOF.
                            drop(stdin.take());
                            let force_shutdown_tx = force_shutdown_tx.clone();
                            tokio::spawn(async move {
                                tokio::time::sleep(HERMES_SHUTDOWN_GRACE).await;
                                let _ = force_shutdown_tx.send(());
                            });
                        } else {
                            let _ = reply.send(());
                        }
                    }
                }
            }
            maybe_inbound = inbound_rx.recv() => {
                let Some(inbound) = maybe_inbound else { break; };
                match inbound {
                    HermesGatewayInbound::StdoutLine(line) => {
                        handle_gateway_stdout_line(
                            &line,
                            &mut pending,
                            &event_tx,
                            &mut ready_tx,
                        );
                    }
                    HermesGatewayInbound::StderrLine(line) => {
                        if ready_tx.is_some() {
                            if startup_stderr.len() == 20 {
                                startup_stderr.pop_front();
                            }
                            startup_stderr.push_back(line.clone());
                        }
                        let _ = event_tx.send(HermesGatewayEvent::Stderr(line));
                    }
                    HermesGatewayInbound::Closed(code) => {
                        for (_id, reply) in pending.drain() {
                            let message = match code {
                                Some(code) => format!("Hermes gateway exited with code {code}"),
                                None => "Hermes gateway exited".to_string(),
                            };
                            let _ = reply.send(Err(HermesRpcError {
                                code: None,
                                message,
                            }));
                        }
                        if let Some(tx) = ready_tx.take() {
                            let mut message = match code {
                                Some(code) => format!("Hermes gateway exited with code {code} before gateway.ready"),
                                None => "Hermes gateway exited before gateway.ready".to_string(),
                            };
                            if !startup_stderr.is_empty() {
                                message.push_str(": ");
                                message.push_str(
                                    &startup_stderr.into_iter().collect::<Vec<_>>().join(" | "),
                                );
                            }
                            let _ = tx.send(Err(message));
                        }
                        let _ = event_tx.send(HermesGatewayEvent::Closed(code));
                        break;
                    }
                }
            }
        }
    }

    for (_id, reply) in pending.drain() {
        let _ = reply.send(Err(HermesRpcError {
            code: None,
            message: "Hermes gateway actor stopped".to_string(),
        }));
    }
    if let Some(reply) = shutdown_reply {
        let _ = reply.send(());
    }
}

fn handle_gateway_stdout_line(
    line: &str,
    pending: &mut HashMap<u64, oneshot::Sender<Result<Value, HermesRpcError>>>,
    event_tx: &mpsc::UnboundedSender<HermesGatewayEvent>,
    ready_tx: &mut Option<oneshot::Sender<Result<(), String>>>,
) {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return;
    }
    let value: Value = match serde_json::from_str(trimmed) {
        Ok(value) => value,
        Err(err) => {
            let _ = event_tx.send(HermesGatewayEvent::ProtocolError(format!(
                "invalid JSON on stdout: {err}: {trimmed}"
            )));
            return;
        }
    };

    if value.get("method").and_then(Value::as_str) == Some("event") {
        match parse_gateway_event(&value) {
            Ok(event) => {
                if matches!(
                    &event,
                    HermesGatewayEvent::Event { event_type, .. } if event_type == "gateway.ready"
                ) && let Some(tx) = ready_tx.take()
                {
                    let _ = tx.send(Ok(()));
                }
                let _ = event_tx.send(event);
            }
            Err(err) => {
                let _ = event_tx.send(HermesGatewayEvent::ProtocolError(err));
            }
        }
        return;
    }

    if let Some(id) = value.get("id").and_then(Value::as_u64) {
        let Some(reply) = pending.remove(&id) else {
            let _ = event_tx.send(HermesGatewayEvent::ProtocolError(format!(
                "Hermes response for unknown request id {id}"
            )));
            return;
        };
        if let Some(error) = value.get("error") {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("Hermes JSON-RPC error")
                .to_string();
            let code = error.get("code").and_then(Value::as_i64);
            let _ = reply.send(Err(HermesRpcError { code, message }));
        } else if let Some(result) = value.get("result") {
            let _ = reply.send(Ok(result.clone()));
        } else {
            let _ = reply.send(Err(HermesRpcError {
                code: None,
                message: format!("Hermes response {id} missing both result and error"),
            }));
        }
        return;
    }

    let _ = event_tx.send(HermesGatewayEvent::ProtocolError(format!(
        "Hermes stdout frame missing method=event or numeric id: {value}"
    )));
}

fn parse_gateway_event(value: &Value) -> Result<HermesGatewayEvent, String> {
    let params = value
        .get("params")
        .and_then(Value::as_object)
        .ok_or_else(|| "Hermes event frame missing params object".to_string())?;
    let event_type = params
        .get("type")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| "Hermes event frame missing non-empty params.type".to_string())?
        .to_string();
    let session_id = params
        .get("session_id")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string);
    let payload = params.get("payload").cloned();
    Ok(HermesGatewayEvent::Event {
        event_type,
        session_id,
        payload,
    })
}

fn spawn_stdout_reader(
    stdout: ChildStdout,
    inbound_tx: mpsc::UnboundedSender<HermesGatewayInbound>,
) {
    tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    if inbound_tx
                        .send(HermesGatewayInbound::StdoutLine(line))
                        .is_err()
                    {
                        break;
                    }
                }
                Ok(None) => break,
                Err(err) => {
                    let _ = inbound_tx.send(HermesGatewayInbound::StderrLine(format!(
                        "failed to read Hermes stdout: {err}"
                    )));
                    break;
                }
            }
        }
    });
}

fn spawn_stderr_reader(
    stderr: ChildStderr,
    inbound_tx: mpsc::UnboundedSender<HermesGatewayInbound>,
) {
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    let trimmed = line.trim();
                    if !trimmed.is_empty()
                        && inbound_tx
                            .send(HermesGatewayInbound::StderrLine(trimmed.to_string()))
                            .is_err()
                    {
                        break;
                    }
                }
                Ok(None) => break,
                Err(err) => {
                    let _ = inbound_tx.send(HermesGatewayInbound::StderrLine(format!(
                        "failed to read Hermes stderr: {err}"
                    )));
                    break;
                }
            }
        }
    });
}

fn spawn_child_waiter(
    mut child: AsyncGroupChild,
    inbound_tx: mpsc::UnboundedSender<HermesGatewayInbound>,
    mut force_shutdown_rx: mpsc::UnboundedReceiver<()>,
) {
    tokio::spawn(async move {
        enum WaitOutcome {
            Exited(std::io::Result<std::process::ExitStatus>),
            ForceShutdown,
        }

        let outcome = tokio::select! {
            status = child.wait() => WaitOutcome::Exited(status),
            _ = force_shutdown_rx.recv() => WaitOutcome::ForceShutdown,
        };
        let code = match outcome {
            WaitOutcome::Exited(status) => status.ok().and_then(|status| status.code()),
            WaitOutcome::ForceShutdown => {
                let _ = child.start_kill();
                child.wait().await.ok().and_then(|status| status.code())
            }
        };
        let _ = inbound_tx.send(HermesGatewayInbound::Closed(code));
    });
}

async fn prepare_hermes_mcp_runtime(
    target: &mut HermesSpawnTarget,
    startup_mcp_servers: &[StartupMcpServer],
    tool_policy: &protocol::ToolPolicy,
    skills_discoverable: bool,
) -> Result<Option<HermesMcpRuntime>, String> {
    let isolate_without_tools = matches!(
        tool_policy,
        protocol::ToolPolicy::AllowList { tools } if tools.is_empty()
    );
    let Some(descriptor) =
        hermes_mcp_bridge_descriptor(startup_mcp_servers, isolate_without_tools)?
    else {
        return Ok(None);
    };
    if target.remote_host.is_some() {
        return Err("Hermes MCP tools are not yet available for SSH-backed workspaces".to_string());
    }

    let bridge_program = resolve_hermes_bridge_executable()?;
    let selected = register_hermes_mcp_bridge(target, &bridge_program).await?;
    let selected_toolsets = if let Some(selected) = selected {
        let selected =
            hermes_selected_toolsets(selected, isolate_without_tools, skills_discoverable);
        let selected = selected.join(",");
        target
            .env
            .insert(HERMES_TOOLSETS_ENV.to_string(), selected.clone());
        target
            .env
            .insert(HERMES_TOOL_PROGRESS_ENV.to_string(), "all".to_string());
        target.args = vec![
            "-c".to_string(),
            HERMES_MCP_GATEWAY_ENTRY.to_string(),
            selected.clone(),
        ];
        Some(selected)
    } else {
        None
    };

    let descriptor_dir = tempfile::Builder::new()
        .prefix("tyde-hermes-mcp-")
        .tempdir()
        .map_err(|error| format!("Failed to create Hermes MCP descriptor directory: {error}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(descriptor_dir.path(), fs::Permissions::from_mode(0o700)).map_err(
            |error| format!("Failed to protect Hermes MCP descriptor directory: {error}"),
        )?;
    }
    let descriptor_path = descriptor_dir.path().join(DESCRIPTOR_FILE_NAME);
    let ready_path = descriptor_dir.path().join(READY_FILE_NAME);
    if let Some(selected_toolsets) = selected_toolsets {
        prepare_hermes_managed_toolsets(descriptor_dir.path(), &selected_toolsets)?;
        target.env.insert(
            HERMES_MANAGED_DIR_ENV.to_string(),
            descriptor_dir.path().to_string_lossy().to_string(),
        );
    }
    let contents = serde_json::to_vec(&descriptor)
        .map_err(|error| format!("Failed to serialize Hermes MCP descriptor: {error}"))?;
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    use std::io::Write as _;
    options
        .open(&descriptor_path)
        .and_then(|mut file| file.write_all(&contents))
        .map_err(|error| format!("Failed to write Hermes MCP descriptor: {error}"))?;
    target.env.insert(
        DESCRIPTOR_ENV.to_string(),
        descriptor_path.to_string_lossy().to_string(),
    );
    target.env.insert(
        "TMPDIR".to_string(),
        descriptor_dir.path().to_string_lossy().to_string(),
    );
    tracing::info!(
        mcp_servers = ?startup_mcp_servers
            .iter()
            .map(|server| server.name.as_str())
            .collect::<Vec<_>>(),
        "Starting Hermes gateway with the managed Tyde MCP bridge"
    );
    Ok(Some(HermesMcpRuntime {
        _descriptor_dir: descriptor_dir,
        ready_path,
    }))
}

fn hermes_selected_toolsets(
    mut selected: Vec<String>,
    isolate_without_tools: bool,
    skills_discoverable: bool,
) -> Vec<String> {
    if isolate_without_tools {
        return vec![MANAGED_SERVER_NAME.to_string()];
    }
    if skills_discoverable && !selected.iter().any(|name| name == "skills") {
        selected.push("skills".to_string());
    }
    if !selected.iter().any(|name| name == MANAGED_SERVER_NAME) {
        selected.push(MANAGED_SERVER_NAME.to_string());
    }
    selected
}

async fn register_hermes_mcp_bridge(
    target: &HermesSpawnTarget,
    bridge_program: &str,
) -> Result<Option<Vec<String>>, String> {
    let mut command = Command::new(&target.program);
    command.args([
        "-c",
        HERMES_BRIDGE_REGISTRATION,
        MANAGED_SERVER_NAME,
        bridge_program,
    ]);
    // Registration must run against the same Hermes home the gateway will be
    // spawned with. `target.env` carries the selected profile's HERMES_HOME,
    // so without this the script reads and writes the *default* profile's
    // `config.yaml` while the gateway reads the named profile's — the managed
    // server is never registered there, Hermes never launches the bridge, and
    // startup dies on the MCP-bridge readiness timeout.
    command.envs(&target.env);
    command.env_remove(HERMES_TOOLSETS_ENV);
    if let Some(path) = process_env::resolved_child_process_path() {
        command.env("PATH", path);
    }
    if let Some(cwd) = target.cwd.as_deref() {
        command.current_dir(cwd);
    }
    let output = command.output().await.map_err(|error| {
        format!(
            "Failed to register the Tyde MCP bridge with Hermes using {}: {error}",
            target.display_program
        )
    })?;
    if String::from_utf8_lossy(&output.stdout).contains(HERMES_MCP_MISSING_MARKER)
        || String::from_utf8_lossy(&output.stderr).contains(HERMES_MCP_MISSING_MARKER)
    {
        return Err(format!(
            "Hermes is installed without its MCP integration (the `mcp` Python package is \
             missing), so it cannot expose the managed Tyde tools. Install it with \
             `{} -m pip install -e '.[mcp]'` from the Hermes agent directory, then relaunch.",
            target.display_program
        ));
    }
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            format!(
                "Hermes rejected Tyde MCP bridge registration with status {}",
                output.status
            )
        } else {
            format!("Hermes rejected Tyde MCP bridge registration: {stderr}")
        });
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let value = stdout
        .lines()
        .chain(stderr.lines())
        .rev()
        .find_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
        .ok_or_else(|| {
            format!(
                "Hermes MCP bridge registration did not report the enabled toolsets; stdout={stdout:?}; stderr={stderr:?}"
            )
        })?;
    if value.is_null() {
        return Ok(None);
    }
    let selected = value
        .as_array()
        .ok_or_else(|| "Hermes MCP bridge registration returned invalid toolsets".to_string())?
        .iter()
        .map(|value| {
            value.as_str().map(str::to_string).ok_or_else(|| {
                "Hermes MCP bridge registration returned a non-string toolset".to_string()
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Some(selected))
}

/// Register Tyde's skill store as an external skills directory for the profile
/// this session will run against, and prove it took.
///
/// Fail-closed: a session that names Tyde skills in its prompt but whose store
/// Hermes cannot see would report every one of them as missing, which is exactly
/// the failure this closes. So a registration that errors, or that comes back
/// without the store in the list, stops the spawn instead of starting a session
/// that lies about what it has.
async fn register_hermes_skill_dir(
    target: &HermesSpawnTarget,
    skills_root: &Path,
) -> Result<(), String> {
    let skills_root = skills_root.to_string_lossy().to_string();
    let mut command = Command::new(&target.program);
    command.args(["-c", HERMES_SKILLS_DIR_REGISTRATION, &skills_root]);
    // Same reason the MCP bridge registration does this: `target.env` carries
    // the selected profile's HERMES_HOME, and without it the script would edit
    // the default profile's `config.yaml` while the gateway reads another's.
    command.envs(&target.env);
    command.env_remove(HERMES_TOOLSETS_ENV);
    if let Some(path) = process_env::resolved_child_process_path() {
        command.env("PATH", path);
    }
    if let Some(cwd) = target.cwd.as_deref() {
        command.current_dir(cwd);
    }
    let output = command.output().await.map_err(|error| {
        format!(
            "Failed to register the Tyde skills directory with Hermes using {}: {error}",
            target.display_program
        )
    })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            format!(
                "Hermes rejected Tyde skills-directory registration with status {}",
                output.status
            )
        } else {
            format!("Hermes rejected Tyde skills-directory registration: {stderr}")
        });
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let registered = stdout
        .lines()
        .chain(stderr.lines())
        .rev()
        .find_map(|line| serde_json::from_str::<Vec<String>>(line.trim()).ok())
        .ok_or_else(|| {
            format!(
                "Hermes skills-directory registration did not report the configured directories; \
                 stdout={stdout:?}; stderr={stderr:?}"
            )
        })?;
    // Compare the way Hermes resolves the list, so a `~`-prefixed entry the user
    // already had counts as the same directory rather than a second one.
    let expanded = expand_hermes_path(&skills_root);
    if !registered
        .iter()
        .any(|entry| expand_hermes_path(entry) == expanded)
    {
        return Err(format!(
            "Hermes did not register the Tyde skills directory {skills_root}; its configured \
             external skill directories are {registered:?}"
        ));
    }
    tracing::debug!(
        skills_root = %skills_root,
        "Hermes discovers Tyde skills through skills.external_dirs"
    );
    Ok(())
}

async fn register_hermes_skill_dirs(
    target: &HermesSpawnTarget,
    skills: &[crate::agent::customization::ResolvedSkill],
) -> Result<(), String> {
    for root in hermes_skill_roots(skills)? {
        register_hermes_skill_dir(target, &root).await?;
    }
    Ok(())
}

fn hermes_skill_roots(
    skills: &[crate::agent::customization::ResolvedSkill],
) -> Result<Vec<PathBuf>, String> {
    let mut roots = Vec::new();
    for skill in skills {
        let root = skill.source_dir.parent().ok_or_else(|| {
            format!(
                "Hermes skill '{}' has no parent store directory: {}",
                skill.name,
                skill.source_dir.display()
            )
        })?;
        if !roots.iter().any(|existing: &PathBuf| existing == root) {
            roots.push(root.to_path_buf());
        }
    }
    Ok(roots)
}

/// Expand a leading `~` the way Hermes' own `Path.expanduser` does, so two
/// spellings of the same directory compare equal.
fn expand_hermes_path(path: &str) -> PathBuf {
    let Some(rest) = path.strip_prefix('~') else {
        return PathBuf::from(path);
    };
    let Ok(home) = crate::paths::home_dir() else {
        return PathBuf::from(path);
    };
    match rest.strip_prefix('/') {
        Some(rest) => home.join(rest),
        // `~otheruser/...` is not this user's home; leave it alone.
        None if !rest.is_empty() => PathBuf::from(path),
        _ => home,
    }
}

fn prepare_hermes_managed_toolsets(
    directory: &Path,
    selected_toolsets: &str,
) -> Result<(), String> {
    let existing = std::env::var_os(HERMES_MANAGED_DIR_ENV)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .filter(|path| path.is_dir())
        .or_else(|| {
            let path = PathBuf::from("/etc/hermes");
            path.is_dir().then_some(path)
        });
    if let Some(existing) = &existing {
        let config = existing.join("config.yaml");
        if config.is_file() {
            fs::copy(&config, directory.join("config.yaml")).map_err(|error| {
                format!(
                    "Failed to preserve Hermes managed configuration from {}: {error}",
                    config.display()
                )
            })?;
        }
    }

    let mut managed_env = existing
        .as_ref()
        .map(|path| path.join(".env"))
        .filter(|path| path.is_file())
        .map(fs::read_to_string)
        .transpose()
        .map_err(|error| format!("Failed to preserve Hermes managed environment: {error}"))?
        .unwrap_or_default();
    if !managed_env.is_empty() && !managed_env.ends_with('\n') {
        managed_env.push('\n');
    }
    managed_env.push_str(HERMES_TOOLSETS_ENV);
    managed_env.push('=');
    managed_env.push_str(selected_toolsets);
    managed_env.push('\n');
    managed_env.push_str(HERMES_TOOL_PROGRESS_ENV);
    managed_env.push_str("=all\n");
    let path = directory.join(".env");
    fs::write(&path, managed_env)
        .map_err(|error| format!("Failed to write Hermes managed tool selection: {error}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
            .map_err(|error| format!("Failed to protect Hermes managed tool selection: {error}"))?;
    }
    Ok(())
}

fn resolve_hermes_bridge_executable() -> Result<String, String> {
    #[cfg(test)]
    if let Some(value) = TEST_HERMES_BRIDGE_EXECUTABLE
        .lock()
        .expect("test Hermes bridge executable mutex poisoned")
        .clone()
    {
        return Ok(value);
    }

    if let Some(value) = std::env::var(HERMES_BRIDGE_EXECUTABLE_ENV)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        let path = PathBuf::from(&value);
        if path.is_file() {
            return Ok(value);
        }
        return Err(format!(
            "{HERMES_BRIDGE_EXECUTABLE_ENV} points to a missing file: {}",
            path.display()
        ));
    }

    let current = std::env::current_exe()
        .map_err(|error| format!("Failed to locate the Tyde server executable: {error}"))?;
    if matches!(
        current.file_stem().and_then(|name| name.to_str()),
        Some("tyde-server" | "tyde" | "Tyde" | "tauri-shell")
    ) {
        return Ok(current.to_string_lossy().to_string());
    }
    if let Some(home) = std::env::var_os("HOME") {
        let installed = PathBuf::from(home)
            .join(".tyde/bin/current")
            .join(if cfg!(windows) {
                "tyde-server.exe"
            } else {
                "tyde-server"
            });
        if installed.is_file() {
            return Ok(installed.to_string_lossy().to_string());
        }
    }
    Err("Could not locate a stable tyde-server executable for the Hermes MCP bridge".to_string())
}

async fn spawn_gateway_child(target: &HermesSpawnTarget) -> Result<AsyncGroupChild, String> {
    if let Some(host) = target.remote_host.as_deref() {
        return crate::remote::spawn_remote_process(
            host,
            &target.program,
            &target.args,
            target.cwd.as_deref(),
        )
        .await;
    }

    let mut command = Command::new(&target.program);
    command.args(&target.args);
    command.env_remove(TYDE_HERMES_SYSTEM_PROMPT_ENV);
    command.envs(&target.env);
    if let Some(path) = process_env::resolved_child_process_path() {
        command.env("PATH", path);
    }
    if let Some(cwd) = target.cwd.as_deref() {
        command.current_dir(cwd);
        command.env("TERMINAL_CWD", cwd);
    }
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .group_spawn()
        .map_err(|err| {
            format!(
                "Failed to spawn Hermes gateway {}: {err}",
                target.display_program
            )
        })
}

impl HermesEventMapper {
    fn drain_background_tasks(&mut self) -> Vec<ChatEvent> {
        let mut tasks = self.background_tasks.drain().collect::<Vec<_>>();
        tasks.sort_by(|(left, _), (right, _)| left.cmp(right));
        tasks
            .into_iter()
            .filter_map(|(task_id, background)| {
                self.background_progress_event(
                    &background,
                    &task_id,
                    BackgroundTaskStatus::Stopped,
                    Some(
                        "Hermes gateway owner exited before the background command reported completion"
                            .to_string(),
                    ),
                )
            })
            .collect()
    }

    fn map_event(&mut self, event_type: &str, payload: Option<Value>) -> Vec<ChatEvent> {
        let result = match event_type {
            "gateway.ready" => Ok(Vec::new()),
            "session.info" => self.map_session_info(payload),
            "session.title" => Ok(Vec::new()),
            "status.update" => self.map_status_update(payload),
            "provider.request.start" => self.map_provider_request_start(payload),
            "message.start" => self.map_message_start(),
            "message.delta" => self.map_message_delta(payload),
            "message.complete" => self.map_message_complete(payload),
            "thinking.delta" | "reasoning.delta" => self.map_reasoning_delta(event_type, payload),
            "reasoning.available" => self.map_reasoning_available(payload),
            "tool.generating" => Ok(Vec::new()),
            "tool.start" => self.map_tool_start(payload),
            "tool.progress" => self.map_tool_progress(payload),
            "tool.complete" => self.map_tool_complete(payload),
            "agent.terminal.output" => self.map_agent_terminal_output(payload),
            "terminal.close" => Ok(Vec::new()),
            "approval.request" => self.map_approval_request(payload),
            "error" => self.map_error(payload),
            event if event.starts_with("subagent.") => Ok(Vec::new()),
            other => Ok(vec![ChatEvent::MessageAdded(warning_message(format!(
                "Hermes event '{other}' is not supported by the Tyde Hermes backend"
            )))]),
        };

        match result {
            Ok(events) => events
                .into_iter()
                .map(|event| normalize_tyde_chat_event(event, &mut self.normalization_failures).0)
                .collect(),
            Err(err) => self.fail_active_turn(err),
        }
    }

    fn fail_active_turn(&mut self, message: impl Into<String>) -> Vec<ChatEvent> {
        let mut events = Vec::new();
        if self.current_message_id.is_some() {
            events.extend(self.finish_stream_events(None, None, None, None));
        }
        events.extend(self.complete_pending_tools_as_cancelled(
            "Hermes protocol error closed the active turn before the tool completed",
        ));
        events.push(ChatEvent::MessageAdded(error_message(message.into())));
        events.push(ChatEvent::TypingStatusChanged(false));
        self.clear_turn_state();
        events
    }

    fn map_session_info(&mut self, payload: Option<Value>) -> Result<Vec<ChatEvent>, String> {
        let payload = required_payload(payload, "session.info")?;
        self.model = optional_string(&payload, &["model"]);
        self.provider = optional_string(&payload, &["provider"]);
        let mut events = Vec::new();
        if let Some(warning) = optional_string(&payload, &["credential_warning"]) {
            events.push(ChatEvent::MessageAdded(warning_message(format!(
                "Hermes credential warning: {warning}"
            ))));
        }
        if !self.session_info_emitted {
            self.session_info_emitted = true;
            let model = self.model.clone().unwrap_or_else(|| "default".to_string());
            let provider = self
                .provider
                .clone()
                .unwrap_or_else(|| "unknown".to_string());
            let cwd = optional_string(&payload, &["cwd"]).unwrap_or_default();
            events.push(ChatEvent::MessageAdded(system_message(format!(
                "Hermes session ready — model: {model}, provider: {provider}, cwd: {cwd}"
            ))));
        }
        Ok(events)
    }

    fn map_status_update(&mut self, payload: Option<Value>) -> Result<Vec<ChatEvent>, String> {
        let payload = required_payload(payload, "status.update")?;
        let text = required_string(&payload, &["text"], "status.update")?;
        let kind = optional_string(&payload, &["kind"]).unwrap_or_else(|| "status".to_string());
        if text == "ready" || text.trim().is_empty() || kind == "process" {
            return Ok(Vec::new());
        }
        if matches!(kind.as_str(), "retry" | "lifecycle") {
            let retry = payload
                .get("attempt")
                .and_then(Value::as_u64)
                .zip(payload.get("max_retries").and_then(Value::as_u64))
                .zip(payload.get("backoff_ms").and_then(Value::as_u64))
                .filter(|((attempt, max_retries), _)| {
                    *attempt > 0 && *max_retries > 0 && *attempt <= *max_retries
                })
                .map(|((attempt, max_retries), backoff_ms)| RetryAttemptData {
                    attempt,
                    max_retries,
                    error: text.clone(),
                    backoff_ms,
                });
            return Ok(retry.map_or_else(
                || {
                    tracing::debug!(
                        status_kind = %kind,
                        status = %text,
                        "Hermes lifecycle status omitted structured retry telemetry"
                    );
                    Vec::new()
                },
                |retry| vec![ChatEvent::RetryAttempt(retry)],
            ));
        }
        if kind == "compacting" {
            tracing::debug!(status = %text, "Hermes is compacting the active context");
            // ChatEvent has no text-only transient/compaction variant. A typing
            // marker would start a Tyde turn and can latch busy after cancel.
            return Ok(Vec::new());
        }
        tracing::debug!(status_kind = %kind, status = %text, "Hermes transient status");
        Ok(Vec::new())
    }

    fn map_message_start(&mut self) -> Result<Vec<ChatEvent>, String> {
        let mut events = Vec::new();
        if self.current_message_id.is_some() {
            events.extend(self.finish_stream_events(None, None, None, None));
            events.push(ChatEvent::MessageAdded(error_message(
                "Hermes emitted message.start before completing the previous message".to_string(),
            )));
        }
        if !self.pending_tools.is_empty() {
            events.extend(self.complete_pending_tools_as_cancelled(
                "Hermes started a new message before the tool completion arrived",
            ));
            events.push(ChatEvent::MessageAdded(error_message(
                "Hermes started a new message with unresolved tool calls".to_string(),
            )));
        }
        self.clear_turn_state();
        let message_id = Uuid::new_v4().to_string();
        self.current_message_id = Some(message_id.clone());
        self.current_text.clear();
        self.current_reasoning_seen = false;
        events.push(ChatEvent::StreamStart(StreamStartData {
            message_id: Some(message_id),
            agent: HERMES_AGENT_NAME.to_string(),
            model: self.model.clone(),
        }));
        Ok(events)
    }

    fn map_provider_request_start(
        &mut self,
        payload: Option<Value>,
    ) -> Result<Vec<ChatEvent>, String> {
        let payload = required_payload(payload, "provider.request.start")?;
        let iteration = payload
            .get("iteration")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                "Hermes provider.request.start missing required integer field iteration".to_string()
            })?;
        if iteration <= 1 {
            return Err(
                "Hermes provider.request.start iteration must be greater than one".to_string(),
            );
        }
        if self.current_message_id.is_none() {
            return Err("Hermes provider request started before message.start".to_string());
        }
        if !self.pending_tools.is_empty() {
            return Err(format!(
                "Hermes provider request started with unresolved tool calls: {}",
                self.pending_tool_ids().join(", ")
            ));
        }

        let (request_usage, cumulative_usage) = payload
            .get("usage")
            .and_then(token_usage_from_value)
            .map(|usage| self.record_session_usage(usage))
            .map_or((None, None), |(request, cumulative)| {
                (Some(request), cumulative)
            });
        let mut events = self.finish_stream_events(None, request_usage, cumulative_usage, None);
        self.turn_tools.clear();
        self.next_turn_tool_order = 0;

        let message_id = Uuid::new_v4().to_string();
        self.current_message_id = Some(message_id.clone());
        events.push(ChatEvent::StreamStart(StreamStartData {
            message_id: Some(message_id),
            agent: HERMES_AGENT_NAME.to_string(),
            model: self.model.clone(),
        }));
        Ok(events)
    }

    fn map_message_delta(&mut self, payload: Option<Value>) -> Result<Vec<ChatEvent>, String> {
        let payload = required_payload(payload, "message.delta")?;
        let text = required_raw_string(&payload, &["text"], "message.delta")?;
        let Some(message_id) = self.current_message_id.clone() else {
            return Err("Hermes emitted message.delta before message.start".to_string());
        };
        if text.is_empty() {
            return Ok(Vec::new());
        }
        self.current_text.push_str(&text);
        Ok(vec![ChatEvent::StreamDelta(StreamTextDeltaData {
            message_id: Some(message_id),
            text,
        })])
    }

    fn map_reasoning_delta(
        &mut self,
        event_type: &str,
        payload: Option<Value>,
    ) -> Result<Vec<ChatEvent>, String> {
        let payload = required_payload(payload, event_type)?;
        let text = required_raw_string(&payload, &["text"], event_type)?;
        if self.current_message_id.is_none() {
            return Err(format!("Hermes emitted {event_type} before message.start"));
        };
        if text.is_empty() {
            return Ok(Vec::new());
        }
        self.current_reasoning_seen = true;
        Ok(Vec::new())
    }

    fn map_reasoning_available(
        &mut self,
        payload: Option<Value>,
    ) -> Result<Vec<ChatEvent>, String> {
        let payload = required_payload(payload, "reasoning.available")?;
        let text = required_raw_string(&payload, &["text"], "reasoning.available")?;
        if text.is_empty() {
            return Ok(Vec::new());
        };
        if self.current_message_id.is_none() {
            return Ok(vec![ChatEvent::MessageAdded(warning_message(
                "Hermes reported reasoning content outside an active message.",
            ))]);
        }
        self.current_reasoning_seen = true;
        Ok(Vec::new())
    }

    fn map_message_complete(&mut self, payload: Option<Value>) -> Result<Vec<ChatEvent>, String> {
        let payload = required_payload(payload, "message.complete")?;
        let status = match optional_raw_string(&payload, &["status"], "message.complete")? {
            Some(raw) if raw.trim().is_empty() => {
                return Err(
                    "Hermes message.complete field status must be non-empty when present"
                        .to_string(),
                );
            }
            Some(raw) => raw.trim().to_string(),
            None => "complete".to_string(),
        };
        if self.awaiting_interrupted_complete && self.current_message_id.is_none() {
            self.awaiting_interrupted_complete = false;
            return Ok(Vec::new());
        }
        if self.current_message_id.is_none() {
            return Err("Hermes emitted message.complete before message.start".to_string());
        }
        let final_text = optional_raw_string(&payload, &["text"], "message.complete")?;
        let usage = payload.get("usage").and_then(token_usage_from_value);
        let cumulative_usage = payload
            .get("cumulative_usage")
            .and_then(token_usage_from_value);
        let context_breakdown = payload
            .get("context_breakdown")
            .and_then(|value| serde_json::from_value::<ContextBreakdown>(value.clone()).ok());
        let stream_final_text = final_text
            .as_ref()
            .filter(|text| !text.trim().is_empty())
            .cloned();
        let has_visible_text = stream_final_text.is_some() || !self.current_text.trim().is_empty();
        let has_reasoning = self.current_reasoning_seen;
        let mut events = Vec::new();
        if !self.pending_tools_finished() && status != "interrupted" {
            events.push(ChatEvent::MessageAdded(error_message(format!(
                "Hermes message.complete arrived with unresolved tool calls: {}",
                self.pending_tool_ids().join(", ")
            ))));
            events.extend(self.complete_pending_tools_as_cancelled(
                "Hermes completed the message before the tool completion arrived",
            ));
        }
        match status.as_str() {
            "interrupted" => {
                events.extend(self.finish_stream_events(
                    stream_final_text,
                    usage,
                    cumulative_usage,
                    context_breakdown,
                ));
                events.extend(self.cancel_events("Operation cancelled"));
            }
            "error" | "failed" => {
                let error_text =
                    optional_string(&payload, &["error"]).or_else(|| stream_final_text.clone());
                if error_text.as_ref().is_some_and(|error| {
                    !self.current_text.trim().is_empty() && self.current_text.trim() == error.trim()
                }) {
                    self.current_text.clear();
                }
                let assistant_final = stream_final_text.clone().filter(|text| {
                    error_text
                        .as_ref()
                        .is_none_or(|error| text.trim() != error.trim())
                });
                events.extend(self.finish_stream_events(
                    assistant_final,
                    usage,
                    cumulative_usage,
                    context_breakdown,
                ));
                if let Some(error_text) = error_text {
                    events.push(ChatEvent::MessageAdded(error_message(error_text)));
                } else {
                    events.push(ChatEvent::MessageAdded(error_message(
                        "Hermes message.complete reported failure without error details.",
                    )));
                }
                events.push(ChatEvent::TypingStatusChanged(false));
            }
            "complete" | "completed" => {
                events.extend(self.finish_stream_events(
                    stream_final_text,
                    usage,
                    cumulative_usage,
                    context_breakdown,
                ));
                if !has_visible_text {
                    if has_reasoning {
                        events.push(ChatEvent::MessageAdded(warning_message(
                            "Hermes completed with reasoning only and no visible assistant text.",
                        )));
                    } else {
                        events.push(ChatEvent::MessageAdded(error_message(
                            "Hermes completed without visible assistant text.",
                        )));
                    }
                }
                events.push(ChatEvent::TypingStatusChanged(false));
            }
            other => {
                events.extend(self.finish_stream_events(
                    stream_final_text,
                    usage,
                    cumulative_usage,
                    context_breakdown,
                ));
                events.push(ChatEvent::MessageAdded(error_message(format!(
                    "Hermes message.complete returned unknown status '{other}'"
                ))));
                events.push(ChatEvent::TypingStatusChanged(false));
            }
        }
        if status != "interrupted" {
            self.clear_turn_state();
        }
        Ok(events)
    }

    fn map_tool_start(&mut self, payload: Option<Value>) -> Result<Vec<ChatEvent>, String> {
        let payload = required_payload(payload, "tool.start")?;
        let tool_call_id =
            required_string_any(&payload, &["tool_id", "tool_call_id"], "tool.start")?;
        let tool_name = required_string_any(&payload, &["name", "tool_name"], "tool.start")?;
        if self.pending_tools.contains_key(&tool_call_id) {
            return Err(format!(
                "Hermes emitted duplicate tool.start for tool_id {tool_call_id}"
            ));
        }
        if self.turn_tools.contains_key(&tool_call_id) {
            return Err(format!(
                "Hermes emitted tool.start for already completed tool_id {tool_call_id}"
            ));
        }
        self.pending_tools
            .insert(tool_call_id.clone(), tool_name.clone());
        self.cancelled_tools.remove(&tool_call_id);
        let content_offset = u32::try_from(self.current_text.chars().count()).ok();
        let observed_order = self.next_turn_tool_order;
        self.next_turn_tool_order = self.next_turn_tool_order.saturating_add(1);
        self.turn_tools.insert(
            tool_call_id.clone(),
            HermesTurnTool {
                name: tool_name.clone(),
                content_offset,
                observed_order,
            },
        );
        let arguments = payload.get("args").cloned().unwrap_or(payload);
        self.pending_tool_arguments
            .insert(tool_call_id.clone(), arguments.clone());
        let tool_type =
            hermes_native_tool_request_type(&tool_name, &arguments).unwrap_or_else(|| {
                ToolRequestType::Other {
                    args: arguments.clone(),
                }
            });
        if is_hermes_delegate_tool(&tool_name) {
            if self.delegation_tools.len() >= 256 {
                self.delegation_tools.pop_front();
            }
            self.delegation_tools.push_back(HermesDelegationTool {
                tool_call_id: tool_call_id.clone(),
                tool_name: tool_name.clone(),
                goals: hermes_delegation_goals(&arguments),
            });
        }
        let mut events = vec![ChatEvent::ToolRequest(ToolRequest {
            tool_call_id: tool_call_id.clone(),
            tool_name: tool_name.clone(),
            tool_type,
        })];
        if let Some(progress) = await_progress_data_for_tool(&tool_call_id, &tool_name, &arguments)
        {
            events.push(ChatEvent::ToolProgress(progress));
        }
        Ok(events)
    }

    fn map_tool_progress(&mut self, payload: Option<Value>) -> Result<Vec<ChatEvent>, String> {
        let payload = required_payload(payload, "tool.progress")?;
        let tool_call_id =
            required_string_any(&payload, &["tool_id", "tool_call_id"], "tool.progress")?;
        // The registered request name is the authority for this id; the
        // payload name is consulted only for ids the registry never saw.
        let tool_name = self
            .pending_tools
            .get(&tool_call_id)
            .cloned()
            .or_else(|| {
                self.turn_tools
                    .get(&tool_call_id)
                    .map(|tool| tool.name.clone())
            })
            .or_else(|| optional_string_any(&payload, &["name", "tool_name"]))
            .ok_or_else(|| {
                format!("Hermes tool.progress missing name for unknown tool_id {tool_call_id}")
            })?;
        Ok(vec![ChatEvent::ToolProgress(ToolProgressData {
            tool_call_id,
            tool_name,
            update: ToolProgressUpdate::Other { payload },
        })])
    }

    fn map_tool_complete(&mut self, payload: Option<Value>) -> Result<Vec<ChatEvent>, String> {
        let payload = required_payload(payload, "tool.complete")?;
        let tool_call_id =
            required_string_any(&payload, &["tool_id", "tool_call_id"], "tool.complete")?;
        let tool_name = required_string_any(&payload, &["name", "tool_name"], "tool.complete")?;
        let Some(expected_name) = self.pending_tools.get(&tool_call_id).cloned() else {
            if self.cancelled_tools.remove(&tool_call_id) {
                return Ok(Vec::new());
            }
            return Err(format!(
                "Hermes emitted tool.complete for tool_id {tool_call_id} with no pending tool.start"
            ));
        };
        if expected_name != tool_name {
            return Err(format!(
                "Hermes tool.complete name mismatch for {tool_call_id}: expected {expected_name}, got {tool_name}"
            ));
        }
        self.pending_tools.remove(&tool_call_id);
        let arguments = self
            .pending_tool_arguments
            .remove(&tool_call_id)
            .unwrap_or(Value::Null);
        let result = payload
            .get("result")
            .cloned()
            .or_else(|| payload.get("summary").cloned())
            .unwrap_or(Value::Null);
        let error = hermes_tool_error(&payload, &result);
        let success = error.is_none();
        let completion_tool_call_id = tool_call_id.clone();
        let mut events = vec![ChatEvent::ToolExecutionCompleted(
            ToolExecutionCompletedData {
                tool_call_id,
                tool_name: tool_name.clone(),
                tool_result: ToolExecutionResult::Other {
                    result: result.clone(),
                },
                success,
                error,
                normalization_failure: None,
            },
        )];
        if success
            && normalized_hermes_tool_name(&tool_name) == "terminal"
            && arguments
                .get("background")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            && let Some(task_id) = non_empty_value_string(&result, &["session_id", "process_id"])
        {
            let command = arguments
                .get("command")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let background = HermesBackgroundTask {
                tool_call_id: completion_tool_call_id.clone(),
                tool_name: tool_name.clone(),
                command,
            };
            events.extend(self.background_progress_event(
                &background,
                &task_id,
                BackgroundTaskStatus::Running,
                None,
            ));
            self.background_tasks.insert(task_id, background);
        }
        if normalized_hermes_tool_name(&tool_name) == "process"
            && arguments.get("action").and_then(Value::as_str) == Some("wait")
            && let Some(task_id) = non_empty_value_string(&arguments, &["session_id", "process_id"])
            && let Some(background) = self.background_tasks.remove(&task_id)
        {
            let exit_code = result.get("exit_code").and_then(Value::as_i64);
            let status = if success && exit_code == Some(0) {
                BackgroundTaskStatus::Completed
            } else {
                BackgroundTaskStatus::Failed
            };
            let summary = exit_code.map(|code| format!("Exited with code {code}"));
            events.extend(self.background_progress_event(&background, &task_id, status, summary));
        }
        if success
            && is_hermes_todo_tool(&tool_name)
            && let Some(tasks) =
                hermes_task_list_from_value(&result, &mut self.task_ids, &mut self.next_task_id)
        {
            tracing::info!(
                tool_call_id = %completion_tool_call_id,
                task_count = tasks.tasks.len(),
                "mapped Hermes todo result to typed task state"
            );
            events.push(ChatEvent::TaskUpdate(tasks));
        }
        if success
            && let Some(progress) =
                spawn_progress_data_for_tool_result(&completion_tool_call_id, &tool_name, &result)
        {
            events.push(ChatEvent::ToolProgress(progress));
        }
        Ok(events)
    }

    fn map_agent_terminal_output(
        &mut self,
        payload: Option<Value>,
    ) -> Result<Vec<ChatEvent>, String> {
        let payload = required_payload(payload, "agent.terminal.output")?;
        let task_id = required_string_any(
            &payload,
            &["process_id", "session_id"],
            "agent.terminal.output",
        )?;
        let Some(background) = self.background_tasks.get(&task_id) else {
            return Ok(Vec::new());
        };
        Ok(self
            .background_progress_event(background, &task_id, BackgroundTaskStatus::Running, None)
            .into_iter()
            .collect())
    }

    /// Background anchors outlive turn resets, so a later turn may reuse the
    /// recorded `tool_call_id` for a different tool. A frame is emitted only
    /// while the id is either unknown to the current turn (the anchor is a
    /// prior turn's card of the recorded name) or still names the recorded
    /// tool; otherwise it is dropped rather than attached to another tool's
    /// card.
    fn background_progress_event(
        &self,
        background: &HermesBackgroundTask,
        task_id: &str,
        status: BackgroundTaskStatus,
        summary: Option<String>,
    ) -> Option<ChatEvent> {
        let current_name = self
            .pending_tools
            .get(&background.tool_call_id)
            .or_else(|| {
                self.turn_tools
                    .get(&background.tool_call_id)
                    .map(|tool| &tool.name)
            });
        if let Some(current_name) = current_name
            && *current_name != background.tool_name
        {
            tracing::debug!(
                tool_call_id = %background.tool_call_id,
                task_id,
                recorded_tool = %background.tool_name,
                current_tool = %current_name,
                "Dropping Hermes background progress: tool_call_id now names a different tool"
            );
            return None;
        }
        Some(ChatEvent::ToolProgress(ToolProgressData {
            tool_call_id: background.tool_call_id.clone(),
            tool_name: background.tool_name.clone(),
            update: ToolProgressUpdate::BackgroundTask(BackgroundTaskState {
                task_id: task_id.to_owned(),
                description: background.command.clone(),
                status,
                summary,
                output_unavailable: None,
            }),
        }))
    }

    /// Resolves the delegation card a native child anchors to: an explicit
    /// parent id wins, then goal text, then being the only still-pending
    /// candidate. With several candidates and no match the child stays
    /// unanchored — guessing would attribute one delegation's children to
    /// another's card.
    fn resolve_delegation_anchor(
        &self,
        payload: &Value,
        description: &str,
    ) -> Option<HermesDelegationAnchor> {
        if let Some(explicit) =
            optional_string_any(payload, &["parent_tool_call_id", "parent_tool_id"])
        {
            return self
                .delegation_tools
                .iter()
                .rev()
                .find(|tool| tool.tool_call_id == explicit)
                .map(HermesDelegationTool::anchor);
        }
        if !description.is_empty()
            && let Some(tool) = self
                .delegation_tools
                .iter()
                .rev()
                .find(|tool| tool.goals.iter().any(|goal| goal == description))
        {
            return Some(tool.anchor());
        }
        let mut outstanding = self
            .delegation_tools
            .iter()
            .filter(|tool| self.pending_tools.contains_key(&tool.tool_call_id));
        match (outstanding.next(), outstanding.next()) {
            (Some(only), None) => Some(only.anchor()),
            _ => None,
        }
    }

    fn map_approval_request(&mut self, payload: Option<Value>) -> Result<Vec<ChatEvent>, String> {
        let payload = required_payload(payload, "approval.request")?;
        if let Some(pending) = self.pending_approval_tool_id.as_ref() {
            return Err(format!(
                "Hermes emitted approval.request while approval {pending} is still pending"
            ));
        }
        self.approval_counter = self.approval_counter.saturating_add(1);
        let tool_call_id = format!("hermes-approval-{}", self.approval_counter);
        let command = optional_string(&payload, &["command"]).unwrap_or_default();
        let description = optional_string(&payload, &["description"])
            .unwrap_or_else(|| "Hermes requests approval".to_string());
        let question = if command.trim().is_empty() {
            description.clone()
        } else {
            format!("{description}\n\nCommand:\n{command}")
        };
        self.pending_approval_tool_id = Some(tool_call_id.clone());
        self.pending_tools
            .insert(tool_call_id.clone(), "approval.request".to_string());
        Ok(vec![ChatEvent::ToolRequest(ToolRequest {
            tool_call_id,
            tool_name: "approval.request".to_string(),
            tool_type: ToolRequestType::ExitPlanMode {
                plan: Some(question),
                plan_path: None,
            },
        })])
    }

    fn map_error(&mut self, payload: Option<Value>) -> Result<Vec<ChatEvent>, String> {
        let payload = required_payload(payload, "error")?;
        let message = optional_string(&payload, &["message"])
            .or_else(|| optional_string(&payload, &["error"]))
            .unwrap_or_else(|| payload.to_string());
        Ok(self.fail_active_turn(message))
    }

    fn record_session_usage(
        &mut self,
        session_usage: TokenUsage,
    ) -> (TokenUsage, Option<TokenUsage>) {
        let (turn_usage, reset) =
            token_usage_delta(self.last_session_usage.as_ref(), &session_usage);
        if reset {
            self.cumulative_usage_incomplete = true;
        }
        self.last_session_usage = Some(session_usage.clone());
        let cumulative_usage = (!self.cumulative_usage_incomplete).then_some(session_usage);
        (turn_usage, cumulative_usage)
    }

    fn finish_stream_events(
        &mut self,
        final_text: Option<String>,
        usage: Option<TokenUsage>,
        cumulative_usage: Option<TokenUsage>,
        context_breakdown: Option<ContextBreakdown>,
    ) -> Vec<ChatEvent> {
        let content = reconcile_hermes_stream_text(&self.current_text, final_text.as_deref());
        let tool_calls = self.tool_uses_for_message(&self.current_text, &content);
        let message_id = self.current_message_id.take().map(protocol::ChatMessageId);
        let reasoning = None;
        self.current_text.clear();
        self.current_reasoning_seen = false;
        let turn_usage = usage;
        let token_usage = match (turn_usage, cumulative_usage) {
            (Some(turn), cumulative) => Some(MessageTokenUsage {
                request: TokenUsageScope::Unavailable {
                    reason: TokenUsageUnavailableReason::ProviderScopeAmbiguous,
                },
                turn: TokenUsageScope::Known {
                    usage: Box::new(turn),
                },
                cumulative: cumulative.map_or(
                    TokenUsageScope::Unavailable {
                        reason: TokenUsageUnavailableReason::ProviderScopeAmbiguous,
                    },
                    |usage| TokenUsageScope::Known {
                        usage: Box::new(usage),
                    },
                ),
            }),
            (None, _) => Some(MessageTokenUsage::unavailable(
                TokenUsageUnavailableReason::BackendDidNotReport,
            )),
        };

        vec![ChatEvent::StreamEnd(StreamEndData {
            message: ChatMessage {
                message_id,
                timestamp: unix_now_ms(),
                sender: MessageSender::Assistant {
                    agent: HERMES_AGENT_NAME.to_string(),
                },
                content,
                reasoning,
                tool_calls,
                model_info: self.model.clone().map(|model| ModelInfo { model }),
                token_usage,
                context_breakdown,
                images: None,
            },
        })]
    }

    fn cancel_events(&mut self, message: &str) -> Vec<ChatEvent> {
        let mut events = Vec::new();
        if self.current_message_id.is_some() {
            events.extend(self.finish_stream_events(None, None, None, None));
        }
        events.extend(
            self.complete_pending_tools_as_cancelled("Tool execution was cancelled by user"),
        );
        events.push(ChatEvent::OperationCancelled(OperationCancelledData {
            message: message.to_string(),
        }));
        events.push(ChatEvent::TypingStatusChanged(false));
        self.current_message_id = None;
        self.current_text.clear();
        self.current_reasoning_seen = false;
        self.clear_turn_tool_state();
        self.awaiting_interrupted_complete = true;
        events
    }

    fn complete_pending_tools_as_cancelled(&mut self, detailed_message: &str) -> Vec<ChatEvent> {
        let pending = self
            .pending_tools
            .iter()
            .map(|(id, name)| (id.clone(), name.clone()))
            .collect::<Vec<_>>();
        let mut events = Vec::new();
        for (tool_call_id, tool_name) in pending {
            self.pending_tools.remove(&tool_call_id);
            self.pending_tool_arguments.remove(&tool_call_id);
            if self.cancelled_tools.len() >= 256 {
                self.cancelled_tools.clear();
            }
            self.cancelled_tools.insert(tool_call_id.clone());
            events.push(ChatEvent::ToolExecutionCompleted(
                ToolExecutionCompletedData {
                    tool_call_id,
                    tool_name,
                    tool_result: ToolExecutionResult::Cancelled {
                        message: detailed_message.to_string(),
                    },
                    success: false,
                    error: Some("Cancelled".to_string()),
                    normalization_failure: None,
                },
            ));
        }
        events
    }

    fn pending_tools_finished(&self) -> bool {
        self.pending_tools.is_empty()
    }

    fn pending_tool_ids(&self) -> Vec<String> {
        self.pending_tools.keys().cloned().collect()
    }

    fn tool_uses_for_message(&self, streamed_text: &str, content: &str) -> Vec<ToolUseData> {
        let mut tools = self.turn_tools.iter().collect::<Vec<_>>();
        tools.sort_by_key(|(_, tool)| {
            (tool.content_offset.unwrap_or(u32::MAX), tool.observed_order)
        });
        tools
            .into_iter()
            .map(|(id, tool)| ToolUseData {
                id: id.clone(),
                name: tool.name.clone(),
                arguments: Value::Null,
                content_offset: tool.content_offset.and_then(|offset| {
                    reanchor_hermes_content_offset(streamed_text, content, offset)
                }),
            })
            .collect()
    }

    fn clear_turn_state(&mut self) {
        self.current_message_id = None;
        self.current_text.clear();
        self.current_reasoning_seen = false;
        self.awaiting_interrupted_complete = false;
        self.clear_turn_tool_state();
    }

    fn clear_turn_tool_state(&mut self) {
        for tool_call_id in self.pending_tools.keys() {
            self.pending_tool_arguments.remove(tool_call_id);
        }
        self.pending_tools.clear();
        self.turn_tools.clear();
        self.next_turn_tool_order = 0;
        self.pending_approval_tool_id = None;
        self.delegation_tools.clear();
    }
}

fn is_hermes_delegate_tool(tool_name: &str) -> bool {
    tool_name
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .map(|ch| ch.to_ascii_lowercase())
        .collect::<String>()
        .ends_with("delegatetask")
}

fn is_hermes_todo_tool(tool_name: &str) -> bool {
    tool_name
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .map(|ch| ch.to_ascii_lowercase())
        .collect::<String>()
        == "todo"
}

fn hermes_task_list_from_value(
    value: &Value,
    task_ids: &mut HashMap<String, u64>,
    next_task_id: &mut u64,
) -> Option<protocol::TaskList> {
    let todos = value.get("todos")?.as_array()?;
    let mut tasks = Vec::with_capacity(todos.len());
    for todo in todos {
        let provider_id = todo.get("id")?.as_str()?.trim();
        let description = todo.get("content")?.as_str()?.trim();
        let status = todo.get("status")?.as_str().and_then(hermes_task_status)?;
        if provider_id.is_empty() || description.is_empty() {
            return None;
        }
        let id = if let Some(id) = task_ids.get(provider_id).copied() {
            id
        } else {
            let id = *next_task_id;
            *next_task_id = next_task_id.saturating_add(1);
            task_ids.insert(provider_id.to_string(), id);
            id
        };
        tasks.push(protocol::Task {
            id,
            description: description.to_string(),
            status,
        });
    }
    Some(protocol::TaskList {
        title: String::new(),
        tasks,
    })
}

fn hermes_task_status(value: &str) -> Option<protocol::TaskStatus> {
    match value.trim().to_ascii_lowercase().as_str() {
        "pending" => Some(protocol::TaskStatus::Pending),
        "in_progress" | "inprogress" | "active" => Some(protocol::TaskStatus::InProgress),
        "completed" | "complete" | "done" => Some(protocol::TaskStatus::Completed),
        "failed" | "cancelled" | "canceled" => Some(protocol::TaskStatus::Failed),
        _ => None,
    }
}

fn hermes_delegation_goals(arguments: &Value) -> Vec<String> {
    arguments
        .get("goals")
        .and_then(Value::as_array)
        .map(|goals| {
            goals
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|goal| !goal.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .or_else(|| {
            arguments
                .get("goal")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|goal| !goal.is_empty())
                .map(|goal| vec![goal.to_owned()])
        })
        .unwrap_or_default()
}

fn hermes_native_tool_request_type(tool_name: &str, arguments: &Value) -> Option<ToolRequestType> {
    let normalized = normalized_hermes_tool_name(tool_name);
    if normalized == "terminal" {
        let command = arguments.get("command")?.as_str()?.to_owned();
        return Some(ToolRequestType::RunCommand {
            command,
            working_directory: arguments
                .get("cwd")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        });
    }
    if is_hermes_delegate_tool(tool_name) {
        let goals = hermes_delegation_goals(arguments);
        return Some(ToolRequestType::AgentSpawn {
            prompt: (!goals.is_empty()).then(|| goals.join("\n\n")),
            name: None,
        });
    }
    None
}

fn normalized_hermes_tool_name(tool_name: &str) -> String {
    tool_name
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .map(|ch| ch.to_ascii_lowercase())
        .collect()
}

fn non_empty_value_string(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        let value = value.get(*key)?;
        if let Some(text) = value.as_str() {
            return non_empty_trimmed(text);
        }
        value
            .as_i64()
            .map(|number| number.to_string())
            .or_else(|| value.as_u64().map(|number| number.to_string()))
    })
}

fn hermes_tool_error(payload: &Value, result: &Value) -> Option<String> {
    let direct = payload
        .get("error")
        .filter(|value| !value.is_null())
        .and_then(hermes_error_text);
    if direct.is_some() {
        return direct;
    }
    let nested = result
        .get("error")
        .filter(|value| !value.is_null())
        .and_then(hermes_error_text);
    if nested.is_some() {
        return nested;
    }
    let is_error = result
        .get("isError")
        .or_else(|| result.get("is_error"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if is_error {
        return hermes_mcp_error_text(result)
            .or_else(|| optional_string_any(result, &["message", "summary"]))
            .or_else(|| Some("Hermes tool reported an error".to_owned()));
    }
    let status = optional_string(result, &["status"]);
    if status.as_deref().is_some_and(|status| {
        matches!(
            status.to_ascii_lowercase().as_str(),
            "blocked" | "cancelled" | "error" | "failed"
        )
    }) {
        return optional_string_any(result, &["message", "summary"])
            .or_else(|| status.map(|status| format!("Hermes tool status: {status}")));
    }
    let exit_code = result.get("exit_code").and_then(Value::as_i64);
    if exit_code.is_some_and(|code| code != 0) {
        return Some(format!(
            "Hermes tool exited with code {}",
            exit_code.expect("nonzero exit code must be present")
        ));
    }
    None
}

fn hermes_error_text(value: &Value) -> Option<String> {
    if let Some(text) = value.as_str() {
        return non_empty_trimmed(text);
    }
    optional_string_any(value, &["message", "detail", "summary"]).or_else(|| {
        let serialized = value.to_string();
        (serialized != "{}" && serialized != "null").then_some(serialized)
    })
}

fn hermes_mcp_error_text(result: &Value) -> Option<String> {
    result
        .get("content")
        .and_then(Value::as_array)
        .and_then(|items| {
            items
                .iter()
                .filter_map(|item| optional_string(item, &["text"]))
                .next()
        })
}

/// Synthetic ids stand in for children the gateway never names. The base id
/// (derived from `task_index`) can be reissued by a later delegate call, so
/// reusing it verbatim would hand a new child a finished child's identity;
/// each new child under a base gets a fresh generation suffix instead.
fn resolve_synthetic_subagent_id(
    base: &str,
    is_live: impl Fn(&str) -> bool,
    issued: &mut HashMap<String, (String, u64)>,
) -> String {
    if let Some((current, _)) = issued.get(base)
        && is_live(current)
    {
        return current.clone();
    }
    let generation = issued
        .get(base)
        .map(|(_, generation)| generation.saturating_add(1))
        .unwrap_or(1);
    let id = if generation == 1 {
        base.to_owned()
    } else {
        format!("{base}-{generation}")
    };
    issued.insert(base.to_owned(), (id.clone(), generation));
    id
}

fn hermes_subagent_progress(
    handle: &SubAgentHandle,
    agent_name: &str,
    anchor: &HermesDelegationAnchor,
    tool_calls: u64,
    completed: bool,
) -> ToolProgressData {
    ToolProgressData {
        tool_call_id: anchor.tool_call_id.clone(),
        tool_name: anchor.tool_name.clone(),
        update: ToolProgressUpdate::SubAgent(protocol::SubAgentProgress {
            agent_id: handle.agent_id.clone(),
            agent_name: agent_name.to_string(),
            last_tool_name: None,
            tool_calls,
            completed,
        }),
    }
}

fn reject_unverified_capabilities(
    config: &BackendSpawnConfig,
    input: &protocol::SendMessagePayload,
) -> Result<(), String> {
    reject_unverified_resume_capabilities(config)?;
    if input
        .images
        .as_ref()
        .is_some_and(|images| !images.is_empty())
    {
        return Err(
            "Hermes image input is disabled until the native gateway contract is verified"
                .to_string(),
        );
    }
    Ok(())
}

fn reject_unverified_resume_capabilities(config: &BackendSpawnConfig) -> Result<(), String> {
    match &config.resolved_spawn_config.tool_policy {
        protocol::ToolPolicy::Unrestricted => {}
        protocol::ToolPolicy::AllowList { tools } if tools.is_empty() => {}
        _ => {
            return Err("Hermes custom tool policies are not enabled because the native gateway policy mapping has not been verified".to_string());
        }
    }
    Ok(())
}

fn hermes_mcp_bridge_descriptor(
    startup_mcp_servers: &[StartupMcpServer],
    force_empty_bridge: bool,
) -> Result<Option<BridgeDescriptor>, String> {
    if startup_mcp_servers.is_empty() && !force_empty_bridge {
        return Ok(None);
    }

    let mut names = std::collections::HashSet::new();
    let mut servers = Vec::new();
    for server in startup_mcp_servers {
        let name = server.name.trim();
        if name.is_empty() {
            return Err("Hermes MCP server name must not be blank".to_string());
        }
        if !names.insert(name.to_string()) {
            return Err(format!("Hermes MCP server name '{name}' is duplicated"));
        }

        let transport = match &server.transport {
            StartupMcpTransport::Stdio { command, args, env } => {
                let command = command.trim();
                if command.is_empty() {
                    return Err(format!(
                        "Hermes MCP server '{name}' stdio command must not be blank"
                    ));
                }
                BridgeTransport::Stdio {
                    command: command.to_string(),
                    args: args.clone(),
                    env: env.clone(),
                }
            }
            StartupMcpTransport::Http {
                url,
                headers,
                bearer_token_env_var,
            } => {
                let url = url.trim();
                if url.is_empty() {
                    return Err(format!(
                        "Hermes MCP server '{name}' HTTP URL must not be blank"
                    ));
                }
                let mut headers = headers.clone();
                if let Some(variable) = bearer_token_env_var
                    .as_ref()
                    .map(|value| value.trim())
                    .filter(|value| !value.is_empty())
                {
                    let token = std::env::var(variable).map_err(|_| {
                        format!(
                            "Hermes MCP server '{name}' requires bearer token environment variable '{variable}'"
                        )
                    })?;
                    headers.retain(|header, _| !header.eq_ignore_ascii_case("authorization"));
                    headers.insert("Authorization".to_string(), format!("Bearer {token}"));
                }
                BridgeTransport::Http {
                    url: url.to_string(),
                    headers,
                }
            }
        };
        servers.push(BridgeServerConfig {
            name: name.to_string(),
            transport,
        });
    }

    Ok(Some(BridgeDescriptor { servers }))
}

fn build_session_create_params(
    workspace_roots: &[String],
    settings: &SessionSettingsValues,
    history_instructions: Option<&str>,
) -> Result<Value, String> {
    let cwd = session_cwd(workspace_roots)?;
    let mut params = json!({
        "cols": 80,
        "source": "tyde",
        "cwd": cwd,
        "close_on_disconnect": false,
    });
    if let Some(instructions) = history_instructions {
        params["messages"] = json!([{ "role": "system", "content": instructions }]);
    }

    // No model/provider baseline is injected here: the selected profile's own
    // Hermes config supplies the defaults, and Tyde manages that config
    // directly. Only an explicit per-session model selection overrides it.
    if let Some(SessionSettingValue::String(model)) = settings.0.get("model") {
        if let Some(selection) = parse_hermes_model_setting(model) {
            params["model"] = Value::String(selection.model);
            if let Some(provider) = selection.provider {
                params["provider"] = Value::String(provider);
            }
        } else if !model.trim().is_empty() {
            return Err(format!("invalid Hermes model setting '{}'", model.trim()));
        }
    }
    if let Some(SessionSettingValue::String(reasoning_effort)) = settings.0.get("reasoning_effort")
        && let Some(reasoning_effort) = non_empty_trimmed(reasoning_effort)
    {
        params["reasoning_effort"] = Value::String(reasoning_effort);
    }
    if let Some(SessionSettingValue::Bool(true)) = settings.0.get("fast") {
        params["fast"] = Value::Bool(true);
    }

    Ok(params)
}

/// Render the session's instructions, naming skills only when Hermes can
/// actually load them.
///
/// `skills_discoverable` is the whole point of the flag: Tyde makes its store
/// visible to Hermes by registering it in the profile's `config.yaml`, which is
/// a file on *this* machine. A gateway running over SSH reads a different
/// machine's config and a different machine's disk, so naming the skills there
/// promises instructions that do not exist — which is precisely how a remote
/// session ends up reporting every selected skill as missing. When they are not
/// discoverable the block is omitted entirely rather than downgraded: a name
/// with nothing behind it is worse than silence.
fn render_hermes_spawn_instructions(
    resolved: &ResolvedSpawnConfig,
    skills_discoverable: bool,
) -> Option<String> {
    let mut without_skills = resolved.clone();
    without_skills.skills.clear();
    let mut sections = render_combined_spawn_instructions(&without_skills)
        .into_iter()
        .collect::<Vec<_>>();
    if skills_discoverable && !resolved.skills.is_empty() {
        let names = resolved
            .skills
            .iter()
            .map(|skill| {
                let name = skill.name.split_whitespace().collect::<Vec<_>>().join(" ");
                format!("- {name}")
            })
            .collect::<Vec<_>>()
            .join("\n");
        // Tyde registers its whole store, not a per-session subset: Hermes reads
        // `skills.external_dirs` per HERMES_HOME, and there is no per-session
        // seam to scope it further. So this names what the agent selected
        // without claiming the others are hidden.
        sections.push(format!(
            "Selected Tyde skills:\n{names}\n\nThese are installed in Tyde's skill store, which \
             this session's Hermes discovers alongside its own; load them with Hermes skill \
             discovery on demand. Other installed skills may also be listed — prefer the ones \
             above. If a selected skill is unavailable, report that instead of inventing its \
             instructions."
        ));
    }
    if sections.is_empty() {
        None
    } else {
        Some(sections.join("\n\n"))
    }
}

/// Decide what this session may name and what it must report, from the outcome
/// of registering Tyde's store with the Hermes install it will run against.
///
/// `registration` is `None` when registration was never attempted — a remote
/// gateway, or a session with no skills. Pure on purpose: the ordering it
/// encodes is load-bearing and would otherwise only be testable through a live
/// gateway. A registration that failed must render instructions that name *no*
/// skills, because a name with nothing behind it makes the model report every
/// selected skill as missing, which is the failure this whole path exists to
/// avoid. The notice is how the user finds out instead.
fn hermes_skill_exposure(
    resolved: &ResolvedSpawnConfig,
    registration: Option<Result<(), String>>,
) -> (Option<String>, Option<String>) {
    let (discoverable, notice) = match registration {
        None => (false, None),
        Some(Ok(())) => (true, None),
        Some(Err(err)) => (
            false,
            Some(format!(
                "Tyde started this Hermes session without its {} selected skill(s): {err}. The \
                 session works normally otherwise.",
                resolved.skills.len()
            )),
        ),
    };
    (
        render_hermes_spawn_instructions(resolved, discoverable),
        notice,
    )
}

/// Decide what a session does about skills it cannot expose remotely.
///
/// Mirrors the policy every other backend uses: the skills are dropped, never
/// the session, whatever the selection asked for. An explicitly selected skill
/// is worth naming more loudly than one of everything-installed, but refusing to
/// start would cost the agent every other skill and the workspace with it.
fn hermes_remote_skill_notice(
    resolved: &ResolvedSpawnConfig,
    remote_host: Option<&str>,
) -> Option<String> {
    let host = remote_host?;
    if resolved.skills.is_empty() {
        return None;
    }
    let selection = match resolved.skill_selection {
        SkillSelection::Explicit => "explicitly selected",
        SkillSelection::AllInstalled => "installed",
    };
    Some(format!(
        "Tyde started this Hermes session without its {} {selection} skill(s): the gateway runs \
         on '{host}' over SSH, and Tyde's skill store is on this machine. The session works \
         normally otherwise, and any skills installed on '{host}' are still available.",
        resolved.skills.len()
    ))
}

pub(crate) fn session_is_resumable_for_workspace_roots(
    workspace_roots: &[String],
    resolved: &ResolvedSpawnConfig,
) -> bool {
    let local = matches!(
        crate::remote::parse_remote_workspace_roots(workspace_roots),
        Ok(None)
    );
    // Skills are only named when they are discoverable, which a remote gateway's
    // never are — so a remote session whose *only* instructions were its skills
    // has nothing left to install and stays resumable.
    render_hermes_spawn_instructions(resolved, local).is_none() || local
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HermesModelSelection {
    model: String,
    provider: Option<String>,
}

/// Encode a model + optional provider as the opaque `SelectOption.value` that
/// round-trips through Tyde's session settings. This is a Tyde-internal
/// transport format (JSON), deliberately independent of the Hermes wire format
/// so an arbitrary model id can never collide with a delimiter.
fn encode_model_option_value(model: &str, provider: Option<&str>) -> String {
    match provider.and_then(non_empty_trimmed) {
        Some(provider) => json!({ "model": model.trim(), "provider": provider }).to_string(),
        None => json!({ "model": model.trim() }).to_string(),
    }
}

fn parse_hermes_model_setting(value: &str) -> Option<HermesModelSelection> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Preferred: structured JSON option value produced by
    // `encode_model_option_value`. Robust to any model id or provider slug.
    if trimmed.starts_with('{') {
        let decoded: Value = serde_json::from_str(trimmed).ok()?;
        let model = decoded
            .get("model")
            .and_then(Value::as_str)
            .and_then(non_empty_trimmed)?;
        let provider = decoded
            .get("provider")
            .and_then(Value::as_str)
            .and_then(non_empty_trimmed);
        return Some(HermesModelSelection { model, provider });
    }
    // Legacy: `"<model> --provider <provider>"` packed string. Retained so
    // settings persisted before the JSON encoding still resolve.
    if let Some((model, provider)) = trimmed.rsplit_once(HERMES_MODEL_PROVIDER_FLAG) {
        let model = model.trim();
        let provider = provider.trim();
        if model.is_empty() || provider.is_empty() {
            return None;
        }
        return Some(HermesModelSelection {
            model: model.to_string(),
            provider: Some(provider.to_string()),
        });
    }
    Some(HermesModelSelection {
        model: trimmed.to_string(),
        provider: None,
    })
}

/// Format a model + provider as the string Hermes `config.set` expects for the
/// `model` key (a CLI-style `"<model> --provider <slug>"`). This is the Hermes
/// wire contract and must not be conflated with `encode_model_option_value`.
fn hermes_model_switch_value(model: &str, provider: Option<&str>) -> String {
    match provider.and_then(non_empty_trimmed) {
        Some(provider) => format!("{}{}{}", model.trim(), HERMES_MODEL_PROVIDER_FLAG, provider),
        None => model.trim().to_string(),
    }
}

fn non_empty_trimmed(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// Parse a `model.options` payload into the per-profile model Select options
/// plus the option matching the profile's current selection.
/// `disabled` holds provider slugs Tyde must not offer for this profile (see
/// [`protocol::HostSettings::hermes_disabled_providers`]). They are dropped
/// after parsing, never before: a malformed provider row is still a malformed
/// payload whether or not the user has hidden that provider.
fn model_select_options_from_payload(
    value: &Value,
    disabled: &[String],
) -> Result<(Vec<SelectOption>, Option<String>), String> {
    let providers = value
        .get("providers")
        .and_then(Value::as_array)
        .ok_or_else(|| "Hermes model.options response missing providers array".to_string())?;
    let current_provider =
        optional_present_non_empty_string(value, &["provider"], "model.options")?;
    let current_model = optional_present_non_empty_string(value, &["model"], "model.options")?;
    let mut model_options = Vec::new();
    let mut model_default = None;

    for (provider_index, provider) in providers.iter().enumerate() {
        if !provider.is_object() {
            return Err(format!(
                "Hermes model.options providers[{provider_index}] must be an object"
            ));
        }
        let provider_context = format!("model.options providers[{provider_index}]");
        let slug = required_non_empty_string(provider, &["slug"], &provider_context)?;
        let label = optional_string(provider, &["name"]).unwrap_or_else(|| slug.clone());
        let authenticated = provider
            .get("authenticated")
            .and_then(Value::as_bool)
            .ok_or_else(|| {
                format!(
                    "Hermes model.options providers[{provider_index}].authenticated must be a bool"
                )
            })?;
        let models = provider
            .get("models")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                format!(
                    "Hermes model.options providers[{provider_index}] '{slug}' missing models array"
                )
            })?;
        for (model_index, model) in models.iter().enumerate() {
            let Some(model) = model.as_str() else {
                return Err(format!(
                    "Hermes model.options providers[{provider_index}] '{slug}' models[{model_index}] must be a string"
                ));
            };
            let Some(model) = non_empty_trimmed(model) else {
                return Err(format!(
                    "Hermes model.options providers[{provider_index}] '{slug}' models[{model_index}] must be non-empty"
                ));
            };
            if !authenticated || disabled.contains(&slug) {
                continue;
            }
            let option_value = encode_model_option_value(&model, Some(&slug));
            if model_default.is_none()
                && current_provider.as_deref() == Some(slug.as_str())
                && current_model.as_deref() == Some(model.as_str())
            {
                model_default = Some(option_value.clone());
            }
            model_options.push(SelectOption {
                value: option_value,
                label: format!("{model} ({label})"),
            });
        }
    }

    if model_options.is_empty() {
        // Say which of the two causes it is. "No models" reads as a broken
        // Hermes install, and sending the user hunting for that when they
        // simply disabled every provider in Tyde would be actively misleading.
        return Err(if disabled.is_empty() {
            "Hermes model.options reported no authenticated providers with selectable models"
                .to_string()
        } else {
            format!(
                "every authenticated Hermes provider is disabled in Tyde for this profile \
                 ({}); re-enable one in the Hermes settings Providers tab",
                disabled.join(", ")
            )
        });
    }

    Ok((model_options, model_default))
}

fn parse_session_create_ids(value: &Value) -> Result<HermesSessionIds, String> {
    let live_session_id = required_string(value, &["session_id"], "session.create")?;
    let stored_session_id = required_string(value, &["stored_session_id"], "session.create")?;
    Ok(HermesSessionIds {
        live_session_id,
        stored_session_id: SessionId(stored_session_id),
    })
}

async fn wait_for_hermes_session_mcp_tools(
    events: &mut mpsc::UnboundedReceiver<HermesGatewayEvent>,
    live_session_id: &str,
) -> Result<Vec<HermesGatewayEvent>, String> {
    let timeout = duration_from_env_ms(HERMES_STARTUP_TIMEOUT_ENV, HERMES_STARTUP_TIMEOUT);
    let deadline = tokio::time::Instant::now() + timeout;
    let mut buffered = Vec::new();
    let mut last_session_info = None;
    loop {
        let event = match tokio::time::timeout_at(deadline, events.recv()).await {
            Ok(Some(event)) => event,
            Ok(None) => {
                return Err(
                    "Hermes gateway event channel closed while waiting for managed MCP tools"
                        .to_string(),
                );
            }
            Err(_) => {
                return Err(format!(
                    "Timed out after {}ms waiting for Hermes to attach the managed Tyde MCP tools{}",
                    timeout.as_millis(),
                    last_session_info
                        .map(|payload: Value| format!("; last session.info payload: {payload}"))
                        .unwrap_or_default()
                ));
            }
        };
        if let HermesGatewayEvent::Event {
            event_type,
            session_id: Some(session_id),
            payload: Some(payload),
        } = &event
            && event_type == "session.info"
            && session_id == live_session_id
        {
            last_session_info = Some(payload.clone());
        }
        let ready = matches!(
            &event,
            HermesGatewayEvent::Event {
                event_type,
                session_id: Some(session_id),
                payload: Some(payload),
            } if event_type == "session.info"
                && session_id == live_session_id
                && payload
                    .get("tools")
                    .and_then(Value::as_object)
                    .is_some_and(session_tools_include_managed_mcp)
        );
        let terminal = match &event {
            HermesGatewayEvent::ProtocolError(error) => Some(format!(
                "Hermes gateway protocol failed while waiting for managed MCP tools: {error}"
            )),
            HermesGatewayEvent::Closed(code) => Some(match code {
                Some(code) => format!(
                    "Hermes gateway exited with code {code} while waiting for managed MCP tools"
                ),
                None => "Hermes gateway exited while waiting for managed MCP tools".to_string(),
            }),
            _ => None,
        };
        buffered.push(event);
        if let Some(error) = terminal {
            return Err(error);
        }
        if ready {
            return Ok(buffered);
        }
    }
}

/// The session has the managed MCP tools either directly (a non-empty
/// mcp-tyde bucket in the model-visible tool map) or via Hermes's Tool
/// Search deferral, where the model-visible list carries the tool_search
/// bridge instead of the deferred MCP tools themselves. Registration of the
/// managed toolset is verified against the gateway's toolset registry before
/// the session is created (`wait_for_hermes_mcp_tools`), so the bridge's
/// presence is sufficient proof of attachment here.
fn session_tools_include_managed_mcp(tools: &serde_json::Map<String, Value>) -> bool {
    if tools
        .get(HERMES_MANAGED_MCP_TOOLSET)
        .and_then(Value::as_array)
        .is_some_and(|tools| !tools.is_empty())
    {
        return true;
    }
    tools.values().any(|bucket| {
        bucket.as_array().is_some_and(|names| {
            names
                .iter()
                .any(|name| name.as_str() == Some(HERMES_TOOL_SEARCH_BRIDGE_TOOL))
        })
    })
}

fn parse_session_list(value: &Value, resumable: bool) -> Result<Vec<BackendSession>, String> {
    let sessions = value
        .get("sessions")
        .and_then(Value::as_array)
        .ok_or_else(|| "Hermes session.list response missing sessions array".to_string())?;
    let mut out = Vec::new();
    for session in sessions {
        let id = required_string(session, &["id"], "session.list session")?;
        let timestamp = session
            .get("started_at")
            .and_then(Value::as_f64)
            .map(timestamp_number_to_ms);
        out.push(BackendSession {
            id: SessionId(id),
            backend_kind: BackendKind::Hermes,
            workspace_roots: Vec::new(),
            title: optional_string(session, &["title"]),
            token_count: None,
            created_at_ms: timestamp,
            updated_at_ms: timestamp,
            resumable,
        });
    }
    Ok(out)
}

fn hermes_history_to_chat_events(value: &Value) -> Result<Vec<ChatEvent>, String> {
    let messages = value
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| "Hermes session.history response missing messages array".to_string())?;
    let mut events = Vec::new();
    let mut task_ids = HashMap::new();
    let mut next_task_id = 0;
    // The assistant record's tool_calls become the replayed ToolRequests, so
    // their names are the authority for each tool_call_id's completion.
    let mut requested_tool_names: HashMap<String, String> = HashMap::new();
    for message in messages {
        let role = required_string(message, &["role"], "session.history message")?;
        let text = message
            .get("text")
            .or_else(|| message.get("content"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let tool_calls = hermes_history_tool_calls(message)?;
        for tool_call in &tool_calls {
            requested_tool_names.insert(tool_call.id.clone(), tool_call.name.clone());
        }
        let sender = match role.as_str() {
            "user" => MessageSender::User,
            "assistant" => MessageSender::Assistant {
                agent: HERMES_AGENT_NAME.to_string(),
            },
            "system" => MessageSender::System,
            "tool" => {
                let Some(tool_call_id) = optional_string_any(message, &["tool_call_id", "tool_id"])
                else {
                    let tool_name = optional_string_any(message, &["tool_name", "name"])
                        .unwrap_or_else(|| "tool".to_string());
                    let context = optional_string_any(message, &["context"]);
                    let content = context
                        .filter(|context| !context.trim().is_empty())
                        .map(|context| format!("Hermes tool {tool_name}: {context}"))
                        .unwrap_or_else(|| format!("Hermes tool: {tool_name}"));
                    events.push(ChatEvent::MessageAdded(system_message(content)));
                    continue;
                };
                let tool_name = requested_tool_names
                    .get(&tool_call_id)
                    .cloned()
                    .or_else(|| optional_string_any(message, &["tool_name", "name"]))
                    .unwrap_or_else(|| "tool".to_string());
                let result =
                    serde_json::from_str(&text).unwrap_or_else(|_| Value::String(text.clone()));
                events.push(ChatEvent::ToolExecutionCompleted(
                    ToolExecutionCompletedData {
                        tool_call_id,
                        tool_name: tool_name.clone(),
                        tool_result: ToolExecutionResult::Other {
                            result: result.clone(),
                        },
                        success: true,
                        error: None,
                        normalization_failure: None,
                    },
                ));
                if is_hermes_todo_tool(&tool_name)
                    && let Some(tasks) =
                        hermes_task_list_from_value(&result, &mut task_ids, &mut next_task_id)
                {
                    events.push(ChatEvent::TaskUpdate(tasks));
                }
                continue;
            }
            other => {
                return Err(format!(
                    "Hermes session.history message has unsupported role '{other}'"
                ));
            }
        };
        if text.trim().is_empty() && tool_calls.is_empty() {
            continue;
        }
        events.push(ChatEvent::MessageAdded(ChatMessage {
            message_id: None,
            timestamp: unix_now_ms(),
            sender,
            content: text,
            reasoning: None,
            tool_calls: tool_calls.clone(),
            model_info: None,
            token_usage: None,
            context_breakdown: None,
            images: None,
        }));
        for tool_call in tool_calls {
            events.push(ChatEvent::ToolRequest(ToolRequest {
                tool_call_id: tool_call.id,
                tool_name: tool_call.name,
                tool_type: ToolRequestType::Other {
                    args: tool_call.arguments,
                },
            }));
        }
    }
    Ok(events)
}

fn hermes_history_tool_calls(message: &Value) -> Result<Vec<ToolUseData>, String> {
    let Some(raw) = message.get("tool_calls").filter(|value| !value.is_null()) else {
        return Ok(Vec::new());
    };
    let parsed = if let Some(text) = raw.as_str() {
        serde_json::from_str::<Value>(text)
            .map_err(|err| format!("Hermes session.history tool_calls is invalid JSON: {err}"))?
    } else {
        raw.clone()
    };
    let calls = parsed
        .as_array()
        .ok_or_else(|| "Hermes session.history tool_calls must be an array".to_string())?;
    calls
        .iter()
        .map(|call| {
            let id = required_string_any(
                call,
                &["id", "call_id", "tool_call_id"],
                "session.history tool call",
            )?;
            let function = call.get("function").unwrap_or(call);
            let name = required_string_any(
                function,
                &["name", "tool_name"],
                "session.history tool call",
            )?;
            let arguments = match function.get("arguments") {
                Some(Value::String(arguments)) => {
                    serde_json::from_str(arguments).map_err(|err| {
                        format!("Hermes session.history tool arguments is invalid JSON: {err}")
                    })?
                }
                Some(arguments) => arguments.clone(),
                None => Value::Null,
            };
            Ok(ToolUseData {
                id,
                name,
                arguments,
                content_offset: None,
            })
        })
        .collect()
}

async fn resolve_gateway_spawn_target(
    workspace_roots: &[String],
) -> Result<HermesSpawnTarget, String> {
    let remote_roots = crate::remote::parse_remote_workspace_roots(workspace_roots)?;
    if let Some((host, roots)) = remote_roots {
        let args = vec!["-m".to_string(), HERMES_PYTHON_MODULE.to_string()];
        let program = std::env::var(HERMES_REMOTE_PYTHON_ENV)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "python3".to_string());
        let cwd = roots.first().cloned();
        return Ok(HermesSpawnTarget {
            display_program: format!("ssh {host} {program} -m {HERMES_PYTHON_MODULE}"),
            program,
            args,
            env: HashMap::new(),
            cwd,
            remote_host: Some(host),
            provider_version: None,
        });
    }

    if test_hermes_python_override_is_set() {
        return hermes_python_spawn_target(resolve_hermes_python_test_override()?, workspace_roots);
    }

    if let Some(program) = explicit_hermes_python() {
        probe_hermes_python_gateway_import(&program)
            .await
            .map_err(|err| err.explicit_override("HERMES_PYTHON").message)?;
        return hermes_python_spawn_target(program, workspace_roots);
    }

    resolve_hermes_cli_gateway_spawn_target(workspace_roots).await
}

fn hermes_python_spawn_target(
    program: String,
    workspace_roots: &[String],
) -> Result<HermesSpawnTarget, String> {
    Ok(HermesSpawnTarget {
        display_program: format!("{program} with Tyde Hermes gateway adapter"),
        program,
        args: vec!["-c".to_string(), HERMES_MCP_GATEWAY_ENTRY.to_string()],
        env: HashMap::new(),
        cwd: Some(session_cwd(workspace_roots)?),
        remote_host: None,
        provider_version: None,
    })
}

async fn resolve_hermes_cli_gateway_spawn_target(
    workspace_roots: &[String],
) -> Result<HermesSpawnTarget, String> {
    if let Some(candidate) = explicit_hermes_executable() {
        return match probe_hermes_cli_gateway(&candidate).await {
            Ok(probe) => {
                let display_program = format!(
                    "{} via {} with Tyde Hermes gateway adapter",
                    probe.executable, probe.gateway_python
                );
                Ok(HermesSpawnTarget {
                    program: probe.gateway_python,
                    args: vec!["-c".to_string(), HERMES_MCP_GATEWAY_ENTRY.to_string()],
                    env: HashMap::new(),
                    cwd: Some(session_cwd(workspace_roots)?),
                    remote_host: None,
                    display_program,
                    provider_version: probe.version,
                })
            }
            Err(err) => Err(err.explicit_override("HERMES_EXECUTABLE").message),
        };
    }

    let mut first_failure = None;
    for candidate in hermes_executable_candidates() {
        match probe_hermes_cli_gateway(&candidate).await {
            Ok(probe) => {
                let display_program = format!(
                    "{} via {} with Tyde Hermes gateway adapter",
                    probe.executable, probe.gateway_python
                );
                return Ok(HermesSpawnTarget {
                    program: probe.gateway_python,
                    args: vec!["-c".to_string(), HERMES_MCP_GATEWAY_ENTRY.to_string()],
                    env: HashMap::new(),
                    cwd: Some(session_cwd(workspace_roots)?),
                    remote_host: None,
                    display_program,
                    provider_version: probe.version,
                });
            }
            Err(err) => {
                tracing::debug!("Hermes executable candidate {candidate} probe failed: {err}");
                if err.code != BackendSetupDiagnosticCode::CommandNotFound || candidate != "hermes"
                {
                    first_failure.get_or_insert(err);
                }
            }
        }
    }
    Err(hermes_cli_required_failure(first_failure).message)
}

fn test_hermes_python_override_is_set() -> bool {
    #[cfg(test)]
    {
        TEST_HERMES_PYTHON
            .lock()
            .expect("test Hermes Python mutex poisoned")
            .is_some()
    }

    #[cfg(not(test))]
    {
        false
    }
}

pub(crate) fn hermes_executable_candidates() -> Vec<String> {
    let mut candidates = Vec::new();

    if let Some(explicit) = explicit_hermes_executable() {
        candidates.push(explicit);
    }

    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        let local = home.join(".local").join("bin").join(HERMES_CLI_BINARY);
        if local.is_file() {
            push_unique_candidate(&mut candidates, local.to_string_lossy().to_string());
        }
    }

    if let Some(path) = process_env::find_executable_in_path(HERMES_CLI_BINARY) {
        push_unique_candidate(&mut candidates, path.to_string_lossy().to_string());
    }

    push_unique_candidate(&mut candidates, HERMES_CLI_BINARY.to_string());
    candidates
}

pub(crate) fn explicit_hermes_executable() -> Option<String> {
    #[cfg(test)]
    if let Some(value) = TEST_HERMES_EXECUTABLE
        .lock()
        .expect("test Hermes executable mutex poisoned")
        .clone()
    {
        return Some(value);
    }

    std::env::var(HERMES_EXECUTABLE_ENV)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

pub(crate) fn explicit_hermes_python() -> Option<String> {
    std::env::var("HERMES_PYTHON")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn push_unique_candidate(candidates: &mut Vec<String>, candidate: String) {
    if !candidates.contains(&candidate) {
        candidates.push(candidate);
    }
}

pub(crate) async fn probe_hermes_cli_gateway(
    command: &str,
) -> Result<HermesCliGatewayProbe, HermesProbeFailure> {
    let output = run_hermes_version_command(command).await?;
    let version = parse_hermes_version_output(&output.stdout, &output.stderr);
    let Some(project_root) = parse_hermes_project_root(&output.stdout, &output.stderr) else {
        return Err(HermesProbeFailure::new(
            BackendSetupDiagnosticCode::MissingProjectRoot,
            format!("Hermes executable {command} --version did not report a Project: root"),
        ));
    };
    let gateway_python =
        resolve_hermes_cli_gateway_python(command, version.as_deref(), &project_root).await?;
    Ok(HermesCliGatewayProbe {
        executable: command.to_string(),
        gateway_python,
        version,
    })
}

async fn resolve_hermes_cli_gateway_python(
    command: &str,
    version: Option<&str>,
    project_root: &Path,
) -> Result<String, HermesProbeFailure> {
    let candidates = hermes_gateway_python_candidates(command, project_root);
    let identity = hermes_cli_identity(command, version, project_root);
    let mut import_failures = Vec::new();

    for candidate in candidates {
        match probe_hermes_python_gateway_import(&candidate.program).await {
            Ok(()) => return Ok(candidate.program),
            Err(err) => import_failures.push((candidate, err)),
        }
    }

    if import_failures.is_empty() {
        return Err(HermesProbeFailure::new(
            BackendSetupDiagnosticCode::MissingGatewayPython,
            format!(
                "{identity}, but Tyde could not resolve a Python interpreter from the Hermes CLI wrapper, console-script shebang, or project virtualenv that can import {HERMES_PYTHON_MODULE}. Remedy: {}",
                hermes_gateway_python_remedy()
            ),
        ));
    }

    let attempts = import_failures
        .into_iter()
        .map(|(candidate, err)| {
            format!(
                "{} from {} failed: {}",
                candidate.program, candidate.source, err.message
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    Err(HermesProbeFailure::new(
        BackendSetupDiagnosticCode::GatewayImportFailed,
        format!(
            "{identity}, but no resolved gateway Python can import {HERMES_PYTHON_MODULE}. {attempts}. Remedy: {}",
            hermes_gateway_python_remedy()
        ),
    ))
}

fn hermes_gateway_python_remedy() -> String {
    format!(
        "Re-run the Hermes installer to restore its Python environment, or set HERMES_PYTHON to a Python interpreter that can import {HERMES_PYTHON_MODULE}."
    )
}

fn hermes_cli_identity(command: &str, version: Option<&str>, project_root: &Path) -> String {
    match version {
        Some(version) => format!(
            "Hermes CLI {command} reported {version} with project {}",
            project_root.display()
        ),
        None => format!(
            "Hermes CLI {command} reported project {}",
            project_root.display()
        ),
    }
}

fn hermes_gateway_python_candidates(
    command: &str,
    project_root: &Path,
) -> Vec<HermesGatewayPythonCandidate> {
    let mut candidates = Vec::new();
    if let Some(path) = local_executable_path_for_inspection(command) {
        collect_python_candidates_from_executable(&path, &mut candidates, &mut Vec::new(), 0);
    }

    for program in hermes_project_python_candidates(project_root) {
        push_unique_python_candidate(
            &mut candidates,
            program,
            format!("Hermes project {}", project_root.display()),
        );
    }

    candidates
}

fn local_executable_path_for_inspection(command: &str) -> Option<PathBuf> {
    let path = Path::new(command);
    if path.components().count() > 1 {
        return path.exists().then(|| path.to_path_buf());
    }

    process_env::find_executable_in_path(command)
}

fn collect_python_candidates_from_executable(
    path: &Path,
    candidates: &mut Vec<HermesGatewayPythonCandidate>,
    visited: &mut Vec<PathBuf>,
    depth: usize,
) {
    if depth > 6 {
        return;
    }

    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if visited.contains(&canonical) {
        return;
    }
    visited.push(canonical.clone());

    let program = canonical.to_string_lossy().to_string();
    if path_looks_like_python(&canonical) {
        push_unique_python_candidate(
            candidates,
            program,
            format!("Hermes CLI wrapper {}", canonical.display()),
        );
    }

    let Ok(contents) = fs::read_to_string(&canonical) else {
        return;
    };

    if let Some(shebang) = contents
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("#!"))
        && let Some(program) = python_from_shebang(shebang)
    {
        push_unique_python_candidate(
            candidates,
            program,
            format!("shebang of {}", canonical.display()),
        );
    }

    for target in executable_targets_from_script(&contents, canonical.parent()) {
        collect_python_candidates_from_executable(&target, candidates, visited, depth + 1);
    }
}

fn push_unique_python_candidate(
    candidates: &mut Vec<HermesGatewayPythonCandidate>,
    program: String,
    source: String,
) {
    if candidates
        .iter()
        .any(|candidate| candidate.program == program)
    {
        return;
    }
    candidates.push(HermesGatewayPythonCandidate { program, source });
}

fn path_looks_like_python(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(|name| name.to_ascii_lowercase().contains("python"))
        .unwrap_or(false)
}

fn python_from_shebang(shebang: &str) -> Option<String> {
    let tokens = split_shell_words(shebang.trim())?;
    if tokens.is_empty() {
        return None;
    }

    if path_looks_like_python(Path::new(&tokens[0])) {
        return Some(tokens[0].clone());
    }

    if Path::new(&tokens[0])
        .file_name()
        .and_then(|name| name.to_str())
        == Some("env")
    {
        let mut iter = tokens.into_iter().skip(1);
        while let Some(token) = iter.next() {
            if token == "-S" {
                let script = iter.collect::<Vec<_>>().join(" ");
                return split_shell_words(&script)?
                    .into_iter()
                    .find(|token| path_looks_like_python(Path::new(token)));
            }
            if token.starts_with('-') {
                continue;
            }
            if path_looks_like_python(Path::new(&token)) {
                return Some(token);
            }
            return None;
        }
    }

    None
}

fn executable_targets_from_script(contents: &str, script_dir: Option<&Path>) -> Vec<PathBuf> {
    contents
        .lines()
        .filter_map(exec_line_tokens)
        .flat_map(|tokens| {
            tokens
                .into_iter()
                .filter_map(move |token| executable_target_from_token(&token, script_dir))
        })
        .collect()
}

fn exec_line_tokens(line: &str) -> Option<Vec<String>> {
    let trimmed = line.trim_start();
    let rest = trimmed.strip_prefix("exec")?;
    if !rest.chars().next().is_some_and(char::is_whitespace) {
        return None;
    }
    let tokens = split_shell_words(rest.trim())?;
    Some(
        tokens
            .into_iter()
            .filter(|token| !skip_exec_token(token))
            .collect(),
    )
}

fn skip_exec_token(token: &str) -> bool {
    token.is_empty()
        || matches!(token, "$@" | "$*" | "${@}" | "${*}")
        || token.starts_with('-')
        || (token.contains('=') && !token.contains('/'))
}

fn executable_target_from_token(token: &str, script_dir: Option<&Path>) -> Option<PathBuf> {
    let expanded = expand_known_shell_vars(token);
    if expanded.is_empty() || expanded.contains('$') {
        return None;
    }

    let path = Path::new(&expanded);
    if path.components().count() > 1 {
        if path.is_absolute() {
            return path.exists().then(|| path.to_path_buf());
        }
        if let Some(script_dir) = script_dir {
            let candidate = script_dir.join(path);
            if candidate.exists() {
                return Some(candidate);
            }
        }
        return path.exists().then(|| path.to_path_buf());
    }

    process_env::find_executable_in_path(&expanded)
}

fn expand_known_shell_vars(token: &str) -> String {
    let mut expanded = token.to_string();
    if let Ok(home) = std::env::var("HOME") {
        expanded = expanded.replace("${HOME}", &home);
        expanded = expanded.replace("$HOME", &home);
    }
    expanded
}

fn split_shell_words(input: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut chars = input.chars().peekable();
    let mut quote = None;
    let mut in_word = false;

    while let Some(ch) = chars.next() {
        match quote {
            Some('\'') => {
                if ch == '\'' {
                    quote = None;
                } else {
                    current.push(ch);
                }
            }
            Some('"') => match ch {
                '"' => quote = None,
                '\\' => {
                    if let Some(next) = chars.next() {
                        current.push(next);
                    }
                }
                _ => current.push(ch),
            },
            Some(_) => return None,
            None => match ch {
                '\'' | '"' => {
                    quote = Some(ch);
                    in_word = true;
                }
                '\\' => {
                    if let Some(next) = chars.next() {
                        current.push(next);
                        in_word = true;
                    }
                }
                ch if ch.is_whitespace() => {
                    if in_word {
                        words.push(std::mem::take(&mut current));
                        in_word = false;
                    }
                }
                '#' if !in_word => break,
                _ => {
                    current.push(ch);
                    in_word = true;
                }
            },
        }
    }

    if quote.is_some() {
        return None;
    }
    if in_word {
        words.push(current);
    }
    Some(words)
}

async fn run_hermes_version_command(
    command: &str,
) -> Result<HermesVersionOutput, HermesProbeFailure> {
    let mut command_proc = Command::new(command);
    command_proc
        .arg("--version")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(path) = process_env::resolved_child_process_path() {
        command_proc.env("PATH", path);
    }
    let mut child = match command_proc.group_spawn() {
        Ok(child) => child,
        Err(err) => {
            let code = if err.kind() == io::ErrorKind::NotFound {
                BackendSetupDiagnosticCode::CommandNotFound
            } else {
                BackendSetupDiagnosticCode::CommandFailed
            };
            return Err(HermesProbeFailure::new(
                code,
                format!("Failed to run Hermes executable {command} --version: {err}"),
            ));
        }
    };
    let mut stdout_pipe = child.inner().stdout.take().ok_or_else(|| {
        HermesProbeFailure::new(
            BackendSetupDiagnosticCode::CommandFailed,
            format!("Failed to capture Hermes {command} --version stdout"),
        )
    })?;
    let mut stderr_pipe = child.inner().stderr.take().ok_or_else(|| {
        HermesProbeFailure::new(
            BackendSetupDiagnosticCode::CommandFailed,
            format!("Failed to capture Hermes {command} --version stderr"),
        )
    })?;
    let status = match tokio::time::timeout(Duration::from_secs(2), child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(err)) => {
            return Err(HermesProbeFailure::new(
                BackendSetupDiagnosticCode::CommandFailed,
                format!("Failed to wait for Hermes {command} --version: {err}"),
            ));
        }
        Err(_) => {
            let _ = child.kill().await;
            return Err(HermesProbeFailure::new(
                BackendSetupDiagnosticCode::CommandTimedOut,
                format!("Timed out probing Hermes executable {command} --version"),
            ));
        }
    };

    let mut stdout_bytes = Vec::new();
    stdout_pipe
        .read_to_end(&mut stdout_bytes)
        .await
        .map_err(|err| {
            HermesProbeFailure::new(
                BackendSetupDiagnosticCode::CommandFailed,
                format!("Failed to read Hermes {command} --version stdout: {err}"),
            )
        })?;
    let mut stderr_bytes = Vec::new();
    stderr_pipe
        .read_to_end(&mut stderr_bytes)
        .await
        .map_err(|err| {
            HermesProbeFailure::new(
                BackendSetupDiagnosticCode::CommandFailed,
                format!("Failed to read Hermes {command} --version stderr: {err}"),
            )
        })?;
    let stdout = String::from_utf8_lossy(&stdout_bytes).into_owned();
    let stderr = String::from_utf8_lossy(&stderr_bytes).into_owned();

    if !status.success() {
        return Err(HermesProbeFailure::new(
            BackendSetupDiagnosticCode::CommandFailed,
            format!(
                "Hermes executable {command} --version exited with status {status}: {}",
                output_preview(&stdout, &stderr)
            ),
        ));
    }

    Ok(HermesVersionOutput { stdout, stderr })
}

fn parse_hermes_version_output(stdout: &str, stderr: &str) -> Option<String> {
    stdout
        .lines()
        .chain(stderr.lines())
        .map(str::trim)
        .find(|line| line.starts_with("Hermes Agent") || line.starts_with("hermes "))
        .map(str::to_string)
}

fn output_preview(stdout: &str, stderr: &str) -> String {
    let combined = stdout
        .lines()
        .chain(stderr.lines())
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" | ");
    if combined.is_empty() {
        "no output".to_string()
    } else {
        combined.chars().take(500).collect()
    }
}

fn parse_hermes_project_root(stdout: &str, stderr: &str) -> Option<PathBuf> {
    stdout
        .lines()
        .chain(stderr.lines())
        .map(str::trim)
        .find_map(|line| line.strip_prefix("Project:").map(str::trim))
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn hermes_project_python_candidates(project_root: &Path) -> Vec<String> {
    #[cfg(windows)]
    let candidates = [
        project_root.join("venv").join("Scripts").join("python.exe"),
        project_root
            .join(".venv")
            .join("Scripts")
            .join("python.exe"),
    ];

    #[cfg(not(windows))]
    let candidates = [
        project_root.join("venv").join("bin").join("python"),
        project_root.join(".venv").join("bin").join("python"),
    ];

    candidates
        .into_iter()
        .filter(|path| path.is_file())
        .map(|path| path.to_string_lossy().to_string())
        .collect()
}

pub(crate) fn hermes_cli_required_failure(
    failure: Option<HermesProbeFailure>,
) -> HermesProbeFailure {
    let action = format!(
        "Install Hermes so `hermes` is on PATH, set HERMES_EXECUTABLE to the Hermes CLI, or set HERMES_PYTHON to a Python interpreter that can import {HERMES_PYTHON_MODULE}"
    );
    match failure {
        Some(failure) if failure.code != BackendSetupDiagnosticCode::CommandNotFound => {
            HermesProbeFailure::new(
                failure.code,
                format!(
                    "Found Hermes CLI, but it is not usable by Tyde: {}",
                    failure.message
                ),
            )
        }
        Some(failure) => HermesProbeFailure::new(
            failure.code,
            format!("Could not find a verified Hermes CLI. {action}"),
        ),
        None => HermesProbeFailure::new(
            BackendSetupDiagnosticCode::CommandNotFound,
            format!("Could not find a verified Hermes CLI. {action}"),
        ),
    }
}

fn resolve_hermes_python_test_override() -> Result<String, String> {
    #[cfg(test)]
    if let Some(value) = TEST_HERMES_PYTHON
        .lock()
        .expect("test Hermes Python mutex poisoned")
        .clone()
    {
        return Ok(value);
    }

    Err("test Hermes Python override is not set".to_string())
}

pub(crate) async fn probe_hermes_python_gateway_import(
    command: &str,
) -> Result<(), HermesProbeFailure> {
    let script = format!(
        "import importlib.util\nimport sys\ntry:\n    spec = importlib.util.find_spec({module:?})\nexcept Exception:\n    spec = None\nsys.exit(0 if spec else 1)\n",
        module = HERMES_PYTHON_MODULE
    );
    let mut command_proc = Command::new(command);
    command_proc
        .arg("-c")
        .arg(script)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(path) = process_env::resolved_child_process_path() {
        command_proc.env("PATH", path);
    }
    let mut child = command_proc.group_spawn().map_err(|err| {
        let code = if err.kind() == io::ErrorKind::NotFound {
            BackendSetupDiagnosticCode::CommandNotFound
        } else {
            BackendSetupDiagnosticCode::CommandFailed
        };
        HermesProbeFailure::new(
            code,
            format!("Failed to run Hermes gateway import probe with {command}: {err}"),
        )
    })?;
    let mut stdout_pipe = child.inner().stdout.take().ok_or_else(|| {
        HermesProbeFailure::new(
            BackendSetupDiagnosticCode::CommandFailed,
            format!("Failed to capture Hermes gateway import probe stdout from {command}"),
        )
    })?;
    let mut stderr_pipe = child.inner().stderr.take().ok_or_else(|| {
        HermesProbeFailure::new(
            BackendSetupDiagnosticCode::CommandFailed,
            format!("Failed to capture Hermes gateway import probe stderr from {command}"),
        )
    })?;
    let status = match tokio::time::timeout(Duration::from_secs(2), child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(err)) => {
            return Err(HermesProbeFailure::new(
                BackendSetupDiagnosticCode::CommandFailed,
                format!("Failed to wait for Hermes gateway import probe with {command}: {err}"),
            ));
        }
        Err(_) => {
            let _ = child.kill().await;
            return Err(HermesProbeFailure::new(
                BackendSetupDiagnosticCode::CommandTimedOut,
                format!("Timed out probing {command} for {HERMES_PYTHON_MODULE}"),
            ));
        }
    };
    let mut stdout_bytes = Vec::new();
    let _ = stdout_pipe.read_to_end(&mut stdout_bytes).await;
    let mut stderr_bytes = Vec::new();
    let _ = stderr_pipe.read_to_end(&mut stderr_bytes).await;
    if status.success() {
        Ok(())
    } else {
        Err(HermesProbeFailure::new(
            BackendSetupDiagnosticCode::GatewayImportFailed,
            format!(
                "Python {command} cannot import {HERMES_PYTHON_MODULE} (probe exited with {status})"
            ),
        ))
    }
}

fn session_cwd(workspace_roots: &[String]) -> Result<String, String> {
    if let Some((_, roots)) = crate::remote::parse_remote_workspace_roots(workspace_roots)? {
        return roots.first().cloned().ok_or_else(|| {
            "Hermes remote session requires at least one remote workspace root".to_string()
        });
    }
    workspace_roots
        .first()
        .cloned()
        .map(Ok)
        .unwrap_or_else(|| tyde_owned_no_root_cwd(HERMES_AGENT_NAME))
}

fn event_targets_session(event_session_id: Option<&str>, live_session_id: &str) -> bool {
    match event_session_id {
        Some(id) => id == live_session_id,
        None => true,
    }
}

fn required_payload(payload: Option<Value>, event_type: &str) -> Result<Value, String> {
    payload.ok_or_else(|| format!("Hermes event {event_type} missing payload"))
}

fn required_string(value: &Value, path: &[&str], context: &str) -> Result<String, String> {
    optional_string(value, path).ok_or_else(|| {
        format!(
            "Hermes {context} missing required string field {}",
            path.join(".")
        )
    })
}

fn required_raw_string(value: &Value, path: &[&str], context: &str) -> Result<String, String> {
    optional_raw_string(value, path, context)?.ok_or_else(|| {
        format!(
            "Hermes {context} missing required string field {}",
            path.join(".")
        )
    })
}

fn required_non_empty_string(
    value: &Value,
    path: &[&str],
    context: &str,
) -> Result<String, String> {
    let raw = required_raw_string(value, path, context)?;
    non_empty_trimmed(&raw).ok_or_else(|| {
        format!(
            "Hermes {context} field {} must be non-empty",
            path.join(".")
        )
    })
}

fn optional_present_non_empty_string(
    value: &Value,
    path: &[&str],
    context: &str,
) -> Result<Option<String>, String> {
    let Some(raw) = optional_raw_string(value, path, context)? else {
        return Ok(None);
    };
    non_empty_trimmed(&raw).map(Some).ok_or_else(|| {
        format!(
            "Hermes {context} field {} must be non-empty",
            path.join(".")
        )
    })
}

fn required_string_any(value: &Value, keys: &[&str], context: &str) -> Result<String, String> {
    optional_string_any(value, keys).ok_or_else(|| {
        format!(
            "Hermes {context} missing required string field; expected one of {}",
            keys.join(", ")
        )
    })
}

fn optional_string(value: &Value, path: &[&str]) -> Option<String> {
    let mut current = value;
    for segment in path {
        current = current.get(*segment)?;
    }
    current
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn optional_raw_string(
    value: &Value,
    path: &[&str],
    context: &str,
) -> Result<Option<String>, String> {
    let mut current = value;
    for segment in path {
        let Some(next) = current.get(*segment) else {
            return Ok(None);
        };
        current = next;
    }
    current
        .as_str()
        .map(|text| Some(text.to_string()))
        .ok_or_else(|| format!("Hermes {context} field {} must be a string", path.join(".")))
}

fn optional_string_any(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| optional_string(value, &[*key]))
}

fn reconcile_hermes_stream_text(streamed: &str, final_text: Option<&str>) -> String {
    let Some(final_text) = final_text.filter(|text| !text.trim().is_empty()) else {
        return streamed.to_string();
    };
    if streamed.trim().is_empty() {
        return final_text.to_string();
    }

    let streamed_trimmed = streamed.trim();
    let final_trimmed = final_text.trim();
    if streamed_trimmed == final_trimmed || streamed_trimmed.ends_with(final_trimmed) {
        return streamed.to_string();
    }
    if final_trimmed.starts_with(streamed_trimmed) {
        return final_text.to_string();
    }
    format!("{}\n\n{}", streamed.trim_end(), final_text.trim_start())
}

fn reanchor_hermes_content_offset(
    streamed_text: &str,
    content: &str,
    observed_offset: u32,
) -> Option<u32> {
    let observed_offset = usize::try_from(observed_offset).ok()?;
    let prefix_end = byte_index_after_chars(streamed_text, observed_offset)?;
    let observed_prefix = &streamed_text[..prefix_end];
    if content.starts_with(observed_prefix) {
        return u32::try_from(observed_offset).ok();
    }
    if observed_prefix.is_empty() {
        return Some(0);
    }

    let mut matches = content.char_indices().filter_map(|(byte_index, _)| {
        content[byte_index..]
            .starts_with(observed_prefix)
            .then_some(byte_index)
    });
    let byte_index = matches.next()?;
    if matches.next().is_some() {
        return None;
    }
    let anchor = content[..byte_index].chars().count();
    u32::try_from(anchor.checked_add(observed_offset)?).ok()
}

fn byte_index_after_chars(text: &str, char_count: usize) -> Option<usize> {
    if char_count == 0 {
        return Some(0);
    }
    text.char_indices()
        .nth(char_count)
        .map(|(byte_index, _)| byte_index)
        .or_else(|| (text.chars().count() == char_count).then_some(text.len()))
}

fn token_usage_from_value(value: &Value) -> Option<TokenUsage> {
    let reports_usage = [
        "input",
        "input_tokens",
        "output",
        "output_tokens",
        "total",
        "total_tokens",
        "cached_prompt_tokens",
        "cache_creation_input_tokens",
        "reasoning_tokens",
        "reasoning",
    ]
    .iter()
    .any(|key| value.get(*key).is_some_and(Value::is_number));
    if !reports_usage {
        return None;
    }
    let input_tokens = value
        .get("input")
        .or_else(|| value.get("input_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output_tokens = value
        .get("output")
        .or_else(|| value.get("output_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let total_tokens = value
        .get("total")
        .or_else(|| value.get("total_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(input_tokens.saturating_add(output_tokens));
    Some(TokenUsage {
        input_tokens,
        output_tokens,
        total_tokens,
        cached_prompt_tokens: value.get("cached_prompt_tokens").and_then(Value::as_u64),
        cache_creation_input_tokens: value
            .get("cache_creation_input_tokens")
            .and_then(Value::as_u64),
        reasoning_tokens: value
            .get("reasoning_tokens")
            .or_else(|| value.get("reasoning"))
            .and_then(Value::as_u64),
    })
}

fn token_usage_to_gateway_value(usage: &TokenUsage) -> Value {
    json!({
        "input": usage.input_tokens,
        "output": usage.output_tokens,
        "total": usage.total_tokens,
        "cached_prompt_tokens": usage.cached_prompt_tokens,
        "cache_creation_input_tokens": usage.cache_creation_input_tokens,
        "reasoning_tokens": usage.reasoning_tokens,
    })
}

fn token_usage_delta(previous: Option<&TokenUsage>, current: &TokenUsage) -> (TokenUsage, bool) {
    let Some(previous) = previous else {
        return (current.clone(), false);
    };
    let reset = current.input_tokens < previous.input_tokens
        || current.output_tokens < previous.output_tokens
        || current.total_tokens < previous.total_tokens
        || optional_counter_decreased(previous.cached_prompt_tokens, current.cached_prompt_tokens)
        || optional_counter_decreased(
            previous.cache_creation_input_tokens,
            current.cache_creation_input_tokens,
        )
        || optional_counter_decreased(previous.reasoning_tokens, current.reasoning_tokens);
    if reset {
        return (current.clone(), true);
    }
    (
        TokenUsage {
            input_tokens: current.input_tokens - previous.input_tokens,
            output_tokens: current.output_tokens - previous.output_tokens,
            total_tokens: current.total_tokens - previous.total_tokens,
            cached_prompt_tokens: optional_token_delta(
                previous.cached_prompt_tokens,
                current.cached_prompt_tokens,
            ),
            cache_creation_input_tokens: optional_token_delta(
                previous.cache_creation_input_tokens,
                current.cache_creation_input_tokens,
            ),
            reasoning_tokens: optional_token_delta(
                previous.reasoning_tokens,
                current.reasoning_tokens,
            ),
        },
        false,
    )
}

fn optional_counter_decreased(previous: Option<u64>, current: Option<u64>) -> bool {
    matches!((previous, current), (Some(previous), Some(current)) if current < previous)
}

fn optional_token_delta(previous: Option<u64>, current: Option<u64>) -> Option<u64> {
    match (previous, current) {
        (Some(previous), Some(current)) => Some(current - previous),
        (None, Some(current)) => Some(current),
        (_, None) => None,
    }
}

fn context_breakdown_from_hermes(value: &Value) -> Option<ContextBreakdown> {
    let input_tokens = value.get("context_used").and_then(Value::as_u64)?;
    let context_window = value
        .get("context_max")
        .and_then(Value::as_u64)
        .filter(|window| *window > 0)?;
    if input_tokens == 0 {
        return None;
    }

    let category_tokens = |ids: &[&str]| {
        value
            .get("categories")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|category| {
                category
                    .get("id")
                    .and_then(Value::as_str)
                    .is_some_and(|id| ids.contains(&id))
            })
            .filter_map(|category| category.get("tokens").and_then(Value::as_u64))
            .fold(0_u64, u64::saturating_add)
    };
    let bytes = |tokens: u64| tokens.saturating_mul(4);
    let system_prompt_tokens = category_tokens(&["system_prompt"]);
    let tool_tokens = category_tokens(&["tool_definitions", "mcp", "subagent_definitions"]);
    let conversation_tokens = category_tokens(&["conversation"]);
    let context_injection_tokens = category_tokens(&["rules", "skills", "memory"]);
    let accounted_tokens = system_prompt_tokens
        .saturating_add(tool_tokens)
        .saturating_add(conversation_tokens)
        .saturating_add(context_injection_tokens);
    let estimated_total = value
        .get("estimated_total")
        .and_then(Value::as_u64)
        .unwrap_or(accounted_tokens);
    if accounted_tokens != estimated_total {
        tracing::debug!(
            input_tokens,
            accounted_tokens,
            estimated_total,
            "Hermes context category estimates do not cover the estimated total"
        );
    }

    Some(ContextBreakdown {
        system_prompt_bytes: bytes(system_prompt_tokens),
        tool_io_bytes: bytes(tool_tokens),
        conversation_history_bytes: bytes(conversation_tokens),
        reasoning_bytes: 0,
        context_injection_bytes: bytes(context_injection_tokens),
        input_tokens,
        context_window,
    })
}

fn user_message(content: &str) -> ChatMessage {
    ChatMessage {
        message_id: None,
        timestamp: unix_now_ms(),
        sender: MessageSender::User,
        content: content.to_string(),
        reasoning: None,
        tool_calls: Vec::new(),
        model_info: None,
        token_usage: None,
        context_breakdown: None,
        images: None,
    }
}

fn system_message(content: impl Into<String>) -> ChatMessage {
    ChatMessage {
        message_id: None,
        timestamp: unix_now_ms(),
        sender: MessageSender::System,
        content: content.into(),
        reasoning: None,
        tool_calls: Vec::new(),
        model_info: None,
        token_usage: None,
        context_breakdown: None,
        images: None,
    }
}

fn warning_message(content: impl Into<String>) -> ChatMessage {
    ChatMessage {
        message_id: None,
        timestamp: unix_now_ms(),
        sender: MessageSender::Warning,
        content: content.into(),
        reasoning: None,
        tool_calls: Vec::new(),
        model_info: None,
        token_usage: None,
        context_breakdown: None,
        images: None,
    }
}

fn error_message(content: impl Into<String>) -> ChatMessage {
    ChatMessage {
        message_id: None,
        timestamp: unix_now_ms(),
        sender: MessageSender::Error,
        content: content.into(),
        reasoning: None,
        tool_calls: Vec::new(),
        model_info: None,
        token_usage: None,
        context_breakdown: None,
        images: None,
    }
}

fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn timestamp_number_to_ms(value: f64) -> u64 {
    if value > 1_000_000_000_000.0 {
        value as u64
    } else {
        (value * 1000.0) as u64
    }
}

fn duration_from_env_ms(key: &str, default: Duration) -> Duration {
    std::env::var(key)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|millis| *millis > 0)
        .map(Duration::from_millis)
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::{BackendAccessMode, SendMessagePayload};
    use std::fs;
    use tempfile::TempDir;
    use tokio::time::timeout;

    struct TestHermesPythonGuard {
        old: Option<String>,
    }

    struct TestHermesExecutableGuard {
        old: Option<String>,
    }

    struct TestHermesBridgeExecutableGuard {
        old: Option<String>,
    }

    struct EnvGuard {
        key: &'static str,
        old_value: Option<String>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let old_value = std::env::var(key).ok();
            unsafe {
                std::env::set_var(key, value);
            }
            Self { key, old_value }
        }

        fn unset(key: &'static str) -> Self {
            let old_value = std::env::var(key).ok();
            unsafe {
                std::env::remove_var(key);
            }
            Self { key, old_value }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match self.old_value.take() {
                Some(value) => unsafe {
                    std::env::set_var(self.key, value);
                },
                None => unsafe {
                    std::env::remove_var(self.key);
                },
            }
        }
    }

    impl TestHermesPythonGuard {
        fn set(value: &str) -> Self {
            let mut guard = TEST_HERMES_PYTHON
                .lock()
                .expect("test Hermes Python mutex poisoned");
            let old = guard.replace(value.to_string());
            Self { old }
        }
    }

    /// Points profile discovery at a test-owned Hermes home so tests never
    /// read the machine's real `~/.hermes`. Serialize with
    /// `TEST_HERMES_OVERRIDE_LOCK` like the other overrides.
    struct TestHermesHomeGuard {
        old: Option<PathBuf>,
    }

    impl TestHermesHomeGuard {
        fn set(path: &Path) -> Self {
            let mut guard = hermes_config::TEST_HERMES_HOME
                .lock()
                .expect("test Hermes home mutex poisoned");
            let old = guard.replace(path.to_path_buf());
            Self { old }
        }
    }

    impl Drop for TestHermesHomeGuard {
        fn drop(&mut self) {
            *hermes_config::TEST_HERMES_HOME
                .lock()
                .expect("test Hermes home mutex poisoned") = self.old.take();
        }
    }

    impl Drop for TestHermesPythonGuard {
        fn drop(&mut self) {
            *TEST_HERMES_PYTHON
                .lock()
                .expect("test Hermes Python mutex poisoned") = self.old.take();
        }
    }

    impl TestHermesExecutableGuard {
        fn set(value: &str) -> Self {
            let mut guard = TEST_HERMES_EXECUTABLE
                .lock()
                .expect("test Hermes executable mutex poisoned");
            let old = guard.replace(value.to_string());
            Self { old }
        }
    }

    impl Drop for TestHermesExecutableGuard {
        fn drop(&mut self) {
            *TEST_HERMES_EXECUTABLE
                .lock()
                .expect("test Hermes executable mutex poisoned") = self.old.take();
        }
    }

    impl TestHermesBridgeExecutableGuard {
        fn set(value: &str) -> Self {
            let mut guard = TEST_HERMES_BRIDGE_EXECUTABLE
                .lock()
                .expect("test Hermes bridge executable mutex poisoned");
            let old = guard.replace(value.to_string());
            Self { old }
        }
    }

    impl Drop for TestHermesBridgeExecutableGuard {
        fn drop(&mut self) {
            *TEST_HERMES_BRIDGE_EXECUTABLE
                .lock()
                .expect("test Hermes bridge executable mutex poisoned") = self.old.take();
        }
    }

    fn payload(message: &str) -> SendMessagePayload {
        SendMessagePayload {
            message: message.to_string(),
            images: None,
            origin: None,
            tool_response: None,
        }
    }

    /// Tyde invokes the Hermes program two ways: as the gateway, and as
    /// `hermes -c <script> <arg>` to edit the profile's config. A fake that only
    /// models the first would answer a registration with gateway chatter, so
    /// every fake gateway answers the skills-directory registration the way a
    /// real install does — by running the script, which reports the configured
    /// directories.
    const FAKE_SKILL_REGISTRATION_PRELUDE: &str = r#"
import json as _json, sys as _sys
if len(_sys.argv) > 3 and _sys.argv[1] == "-c" and "external_dirs" in _sys.argv[2]:
    print(_json.dumps([_sys.argv[3]]), flush=True)
    raise SystemExit(0)
"#;

    fn write_fake_gateway(dir: &TempDir, body: &str) -> String {
        let script = dir.path().join("fake_gateway.py");
        fs::write(&script, format!("{FAKE_SKILL_REGISTRATION_PRELUDE}{body}"))
            .expect("write fake gateway");
        let launcher = dir.path().join("fake_python.sh");
        fs::write(
            &launcher,
            format!("#!/bin/sh\nexec python3 {} \"$@\"\n", script.display()),
        )
        .expect("write fake python");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&launcher)
                .expect("launcher metadata")
                .permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&launcher, perms).expect("chmod launcher");
        }
        launcher.to_string_lossy().to_string()
    }

    fn write_fake_managed_mcp_gateway(
        dir: &TempDir,
    ) -> (String, std::path::PathBuf, std::path::PathBuf) {
        write_fake_managed_mcp_gateway_with_deferral(dir, false)
    }

    /// Like `write_fake_managed_mcp_gateway`, but simulates a Hermes with
    /// Tool Search deferral active: tools.show never names the managed
    /// toolset (the model-visible list carries only the tool_search bridge),
    /// and the managed toolset is visible solely through the tools.list
    /// registry view.
    fn write_fake_deferred_mcp_gateway(
        dir: &TempDir,
    ) -> (String, std::path::PathBuf, std::path::PathBuf) {
        write_fake_managed_mcp_gateway_with_deferral(dir, true)
    }

    fn write_fake_managed_mcp_gateway_with_deferral(
        dir: &TempDir,
        deferred: bool,
    ) -> (String, std::path::PathBuf, std::path::PathBuf) {
        let observed = dir.path().join("observed.json");
        let config = dir.path().join("config.json");
        fs::write(
            &config,
            serde_json::to_vec(&json!({
                "model": { "default": "native-model" },
                "mcp_servers": {
                    "native": { "command": "native-command", "args": [] },
                    "tyde": {
                        "command": "/obsolete/tyde-server",
                        "args": ["hermes-mcp-bridge"]
                    }
                }
            }))
            .expect("serialize fake config"),
        )
        .expect("write fake config");
        let observed_json =
            serde_json::to_string(&observed.to_string_lossy()).expect("serialize observed path");
        let config_json =
            serde_json::to_string(&config.to_string_lossy()).expect("serialize config path");
        let deferred_py = if deferred { "True" } else { "False" };
        let script = format!(
            r#"
import json, os, sys

observed_path = {observed_json}
config_path = {config_json}
DEFERRED = {deferred_py}
if len(sys.argv) == 5 and sys.argv[1:2] == ["-c"]:
    name = sys.argv[-2]
    command = sys.argv[-1]
    with open(config_path, encoding="utf-8") as source:
        config = json.load(source)
    existing = config.setdefault("mcp_servers", {{}}).get(name)
    managed = {{"command": command, "args": ["hermes-mcp-bridge"]}}
    if existing != managed:
        if existing is not None and existing.get("args") != ["hermes-mcp-bridge"]:
            raise RuntimeError("user-managed collision")
        config["mcp_servers"][name] = managed
        with open(config_path, "w", encoding="utf-8") as output:
            json.dump(config, output, sort_keys=True)
    print(json.dumps(["file"]))
    raise SystemExit(0)

with open(config_path, encoding="utf-8") as source:
    config = json.load(source)
with open(os.environ["TYDE_HERMES_MCP_DESCRIPTOR"], encoding="utf-8") as source:
    descriptor = json.load(source)
with open(observed_path, "w", encoding="utf-8") as output:
    json.dump({{
        "config": config,
        "descriptor": descriptor,
        "toolsets": os.environ.get("HERMES_TUI_TOOLSETS")
    }}, output, sort_keys=True)
with open(os.path.join(os.environ["TMPDIR"], "tyde-mcp-ready.json"), "w", encoding="utf-8") as output:
    json.dump({{"ok": True}}, output)
print(json.dumps({{"jsonrpc":"2.0","method":"event","params":{{"type":"gateway.ready","payload":{{"skin":"default"}}}}}}), flush=True)
for line in sys.stdin:
    request = json.loads(line)
    request_id = request["id"]
    method = request["method"]
    params = request.get("params") or {{}}
    if method == "session.create":
        result = {{"session_id":"live-mcp","stored_session_id":"stored-mcp","messages":[],"info":{{}}}}
    elif method == "prompt.submit":
        result = {{"status":"streaming"}}
    elif method == "session.usage":
        result = {{"input":0,"output":0,"total":0}}
    elif method == "tools.show":
        if DEFERRED:
            result = {{"sections":[{{"name":"unknown","tools":[{{"name":"tool_search"}},{{"name":"tool_describe"}},{{"name":"tool_call"}}]}}],"total":3}}
        else:
            result = {{"sections":[{{"name":"mcp-tyde","tools":[{{"name":"mcp_tyde_probe"}}]}}],"total":1}}
    elif method == "tools.list":
        if DEFERRED:
            result = {{"toolsets":[
                {{"name":"file","tool_count":1,"enabled":True,"tools":["read_file"]}},
                {{"name":"tyde","tool_count":1,"enabled":True,"tools":["mcp_tyde_probe"]}},
            ]}}
        else:
            result = {{"toolsets":[]}}
    else:
        result = {{}}
    print(json.dumps({{"jsonrpc":"2.0","id":request_id,"result":result}}), flush=True)
    if method == "session.create":
        if DEFERRED:
            tools_payload = {{"other":["tool_search","tool_describe","tool_call"]}}
        else:
            tools_payload = {{"mcp-tyde":["mcp_tyde_probe"]}}
        print(json.dumps({{"jsonrpc":"2.0","method":"event","params":{{"type":"session.info","session_id":"live-mcp","payload":{{"tools":tools_payload}}}}}}), flush=True)
    if method == "prompt.submit":
        session_id = params["session_id"]
        print(json.dumps({{"jsonrpc":"2.0","method":"event","params":{{"type":"message.start","session_id":session_id}}}}), flush=True)
        print(json.dumps({{"jsonrpc":"2.0","method":"event","params":{{"type":"message.complete","session_id":session_id,"payload":{{"text":"mcp ready","status":"complete"}}}}}}), flush=True)
"#
        );
        (write_fake_gateway(dir, &script), observed, config)
    }

    fn write_fake_hermes_cli_install(dir: &TempDir) -> (String, String) {
        let project = dir.path().join("hermes-agent");
        fs::create_dir_all(&project).expect("create fake Hermes project");
        let python = dir.path().join("fake_python");
        let console = dir.path().join("hermes_console");
        fs::write(
            &python,
            "#!/bin/sh\nif [ \"$1\" = \"-c\" ]; then exit 0; fi\nexit 1\n",
        )
        .expect("write fake Hermes Python");
        fs::write(
            &console,
            format!("#!{}\nimport sys\nsys.exit(1)\n", python.to_string_lossy()),
        )
        .expect("write fake Hermes console script");
        let hermes = dir.path().join("hermes");
        let console_quoted = console.to_string_lossy().replace('\'', "'\\''");
        fs::write(
            &hermes,
            format!(
                "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then\n  printf 'Hermes Agent v9.9.9\\nProject: {}\\n'\n  exit 0\nfi\nexec '{console_quoted}' \"$@\"\n",
                project.to_string_lossy(),
            ),
        )
        .expect("write fake Hermes executable");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for path in [&python, &console, &hermes] {
                let mut perms = fs::metadata(path)
                    .expect("fake Hermes metadata")
                    .permissions();
                perms.set_mode(0o755);
                fs::set_permissions(path, perms).expect("chmod fake Hermes executable");
            }
        }
        (
            hermes.to_string_lossy().to_string(),
            python.to_string_lossy().to_string(),
        )
    }

    #[tokio::test]
    async fn hermes_spawn_target_prefers_verified_cli_install() {
        let _test_lock = TEST_HERMES_OVERRIDE_LOCK.lock().await;
        let dir = TempDir::new().expect("tempdir");
        let (hermes, python) = write_fake_hermes_cli_install(&dir);
        let _hermes_guard = TestHermesExecutableGuard::set(&hermes);
        let _python_env = EnvGuard::unset("HERMES_PYTHON");

        let target = resolve_gateway_spawn_target(&[dir.path().to_string_lossy().to_string()])
            .await
            .expect("resolve Hermes spawn target");

        assert_eq!(target.program, python);
        assert_eq!(target.args[0], "-c");
        assert_eq!(target.args[1], HERMES_MCP_GATEWAY_ENTRY);
        assert!(
            target.display_program.contains(&hermes),
            "display should mention resolved Hermes executable: {}",
            target.display_program
        );
    }

    #[tokio::test]
    async fn hermes_spawn_target_discovers_home_local_bin_cli() {
        let _test_lock = TEST_HERMES_OVERRIDE_LOCK.lock().await;
        let home = TempDir::new().expect("tempdir");
        let local_bin = home.path().join(".local").join("bin");
        fs::create_dir_all(&local_bin).expect("create fake local bin");
        let cli_dir = TempDir::new().expect("cli tempdir");
        let (hermes, python) = write_fake_hermes_cli_install(&cli_dir);
        let local_hermes = local_bin.join("hermes");
        fs::rename(&hermes, &local_hermes).expect("move fake Hermes into ~/.local/bin");
        let _home = EnvGuard::set("HOME", &home.path().to_string_lossy());
        let _python_env = EnvGuard::unset("HERMES_PYTHON");
        let _executable_env = EnvGuard::unset("HERMES_EXECUTABLE");

        let target = resolve_gateway_spawn_target(&[home.path().to_string_lossy().to_string()])
            .await
            .expect("resolve Hermes spawn target");

        assert_eq!(target.program, python);
        let local_hermes = local_hermes.to_string_lossy();
        assert!(
            target.display_program.contains(local_hermes.as_ref()),
            "display should mention Hermes discovered in ~/.local/bin: {}",
            target.display_program
        );
    }

    #[tokio::test]
    async fn hermes_gateway_import_failure_is_concise() {
        let dir = TempDir::new().expect("tempdir");
        let python = dir.path().join("python");
        fs::write(
            &python,
            "#!/bin/sh\nprintf 'Traceback (most recent call last):\\nModuleNotFoundError: tui_gateway\\n' >&2\nexit 1\n",
        )
        .expect("write fake python");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&python)
                .expect("fake python metadata")
                .permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&python, perms).expect("chmod fake python");
        }

        let failure = probe_hermes_python_gateway_import(&python.to_string_lossy())
            .await
            .expect_err("import probe should fail");

        assert_eq!(
            failure.code,
            BackendSetupDiagnosticCode::GatewayImportFailed
        );
        assert!(
            !failure.message.contains("Traceback")
                && !failure.message.contains("ModuleNotFoundError"),
            "diagnostic should not include raw Python traceback output: {}",
            failure.message
        );
    }

    #[tokio::test]
    async fn hermes_backend_maps_basic_turn() {
        let _test_lock = TEST_HERMES_OVERRIDE_LOCK.lock().await;
        let dir = TempDir::new().expect("tempdir");
        let fake = write_fake_gateway(
            &dir,
            r#"
import json, sys, threading, time
sessions = {}
print(json.dumps({"jsonrpc":"2.0","method":"event","params":{"type":"gateway.ready","payload":{"skin":"default"}}}), flush=True)

def emit(t, sid, payload=None):
    params = {"type": t, "session_id": sid}
    if payload is not None:
        params["payload"] = payload
    print(json.dumps({"jsonrpc":"2.0","method":"event","params":params}), flush=True)

for line in sys.stdin:
    req = json.loads(line)
    rid = req["id"]
    method = req["method"]
    params = req.get("params") or {}
    if method == "session.create":
        sid = "live1"
        sessions[sid] = 0
        print(json.dumps({"jsonrpc":"2.0","id":rid,"result":{"session_id":sid,"stored_session_id":"stored1","messages":[],"info":{}}}), flush=True)
        emit("session.info", sid, {"model":"fake-model","provider":"fake","cwd":"/tmp"})
    elif method == "prompt.submit":
        sid = params["session_id"]
        sessions[sid] = sessions.get(sid, 0) + 1
        turn = sessions[sid]
        print(json.dumps({"jsonrpc":"2.0","id":rid,"result":{"status":"streaming"}}), flush=True)
        emit("message.start", sid)
        emit("reasoning.delta", sid, {"text":"think"})
        emit("message.delta", sid, {"text":"hel"})
        emit("message.delta", sid, {"text":"lo"})
        emit("message.complete", sid, {"text":"hello","status":"complete","usage":{"input":turn,"output":2*turn,"total":3*turn,"cached_prompt_tokens":10*turn,"cache_creation_input_tokens":4*turn,"reasoning_tokens":turn}})
    elif method == "session.interrupt":
        print(json.dumps({"jsonrpc":"2.0","id":rid,"result":{"status":"interrupted"}}), flush=True)
    elif method == "session.usage":
        print(json.dumps({"jsonrpc":"2.0","id":rid,"result":{"input":1,"output":2,"total":3}}), flush=True)
    elif method == "session.context_breakdown":
        print(json.dumps({"jsonrpc":"2.0","id":rid,"result":{"context_used":12000,"context_max":200000,"estimated_total":12000,"categories":[{"id":"system_prompt","tokens":1000},{"id":"tool_definitions","tokens":2000},{"id":"conversation","tokens":9000}]}}), flush=True)
    elif method == "session.history":
        print(json.dumps({"jsonrpc":"2.0","id":rid,"result":{"count":0,"messages":[]}}), flush=True)
    elif method == "session.list":
        print(json.dumps({"jsonrpc":"2.0","id":rid,"result":{"sessions":[]}}), flush=True)
    else:
        print(json.dumps({"jsonrpc":"2.0","id":rid,"result":{}}), flush=True)
"#,
        );
        let _guard = TestHermesPythonGuard::set(&fake);
        let (backend, mut events) = HermesBackend::spawn(
            vec![dir.path().to_string_lossy().to_string()],
            BackendSpawnConfig::default(),
            payload("hello"),
        )
        .await
        .expect("spawn fake hermes");
        assert_eq!(backend.session_id(), SessionId("stored1".to_string()));

        let mut saw_start = false;
        let mut text = String::new();
        let mut saw_end = false;
        let mut observed = Vec::new();
        let deadline = Duration::from_secs(2);
        while !saw_end {
            let event = timeout(deadline, events.recv())
                .await
                .expect("event timeout")
                .expect("event stream open");
            observed.push(format!("{event:?}"));
            match event {
                ChatEvent::StreamStart(_) => saw_start = true,
                ChatEvent::StreamReasoningDelta(delta) => {
                    panic!("Hermes raw reasoning must not be emitted: {delta:?}");
                }
                ChatEvent::StreamDelta(delta) => text.push_str(&delta.text),
                ChatEvent::StreamEnd(end) => {
                    assert_eq!(end.message.content, "hello");
                    assert!(end.message.reasoning.is_none());
                    let context = end
                        .message
                        .context_breakdown
                        .as_ref()
                        .expect("context breakdown");
                    assert_eq!(context.input_tokens, 12_000);
                    assert_eq!(context.context_window, 200_000);
                    let usage = end.message.token_usage.as_ref().expect("usage");
                    assert_eq!(
                        usage.turn.known_usage().expect("turn usage").total_tokens,
                        3
                    );
                    assert_eq!(
                        usage
                            .turn
                            .known_usage()
                            .and_then(|usage| usage.cached_prompt_tokens),
                        Some(10)
                    );
                    saw_end = true;
                }
                _ => {}
            }
        }
        assert!(saw_start);
        assert_eq!(text, "hello");
        assert!(
            observed.iter().all(|event| !event.contains("think")),
            "raw Hermes reasoning leaked into events: {observed:#?}"
        );

        assert!(
            backend
                .send(AgentInput::SendMessage(payload("again")))
                .await
        );
        let second_end = timeout(deadline, async {
            loop {
                if let Some(ChatEvent::StreamEnd(end)) = events.recv().await {
                    break end;
                }
            }
        })
        .await
        .expect("second turn timeout");
        let second_usage = second_end.message.token_usage.expect("second usage");
        assert_eq!(
            second_usage
                .turn
                .known_usage()
                .expect("second turn usage")
                .total_tokens,
            3
        );
        assert_eq!(
            second_usage
                .cumulative
                .known_usage()
                .expect("second cumulative usage")
                .total_tokens,
            6
        );
        assert_eq!(
            second_usage
                .cumulative
                .known_usage()
                .and_then(|usage| usage.cached_prompt_tokens),
            Some(20)
        );
        backend.shutdown().await;
    }

    #[tokio::test]
    async fn hermes_rejects_images_until_verified() {
        let mut with_image = payload("hello");
        with_image.images = Some(vec![protocol::ImageData {
            media_type: "image/png".to_string(),
            data: "abc".to_string(),
        }]);
        let err = match HermesBackend::spawn(Vec::new(), BackendSpawnConfig::default(), with_image)
            .await
        {
            Ok(_) => panic!("image support should be disabled"),
            Err(err) => err,
        };
        assert!(err.contains("image input is disabled"));
    }

    #[tokio::test]
    async fn hermes_gateway_uses_managed_mcp_bridge() {
        let _test_lock = TEST_HERMES_OVERRIDE_LOCK.lock().await;
        let dir = TempDir::new().expect("tempdir");
        let (fake, observed_path, config_path) = write_fake_managed_mcp_gateway(&dir);
        let _python_guard = TestHermesPythonGuard::set(&fake);
        let _token_guard = EnvGuard::set("TYDE_MCP_TOKEN", "private-token");
        let bridge = dir.path().join("tyde-server");
        fs::write(&bridge, "bridge placeholder").expect("write bridge placeholder");
        let _bridge_guard = TestHermesBridgeExecutableGuard::set(&bridge.to_string_lossy());

        let startup_mcp_servers = vec![
            StartupMcpServer {
                name: "local".to_string(),
                transport: StartupMcpTransport::Stdio {
                    command: "node".to_string(),
                    args: vec!["server.js".to_string()],
                    env: HashMap::from([("LOCAL_KEY".to_string(), "value".to_string())]),
                },
            },
            StartupMcpServer {
                name: "shared".to_string(),
                transport: StartupMcpTransport::Http {
                    url: "https://tyde.invalid/mcp".to_string(),
                    headers: HashMap::from([("X-Tyde".to_string(), "yes".to_string())]),
                    bearer_token_env_var: Some("TYDE_MCP_TOKEN".to_string()),
                },
            },
        ];

        let config = BackendSpawnConfig {
            startup_mcp_servers,
            ..BackendSpawnConfig::default()
        };
        let (backend, mut events) = HermesBackend::spawn(
            vec![dir.path().to_string_lossy().to_string()],
            config,
            payload("verify MCP startup"),
        )
        .await
        .expect("spawn bridged fake Hermes backend");

        timeout(Duration::from_secs(2), async {
            while let Some(event) = events.recv().await {
                if matches!(event, ChatEvent::StreamEnd(_)) {
                    return;
                }
            }
            panic!("Hermes event stream closed before the MCP test turn completed");
        })
        .await
        .expect("MCP test turn should finish");

        let observed: Value = serde_json::from_slice(
            &fs::read(&observed_path).expect("read observed process config"),
        )
        .expect("parse observed process config");
        assert_eq!(observed["toolsets"], "file,tyde");
        assert_eq!(
            observed["config"]["mcp_servers"]["native"]["command"],
            "native-command"
        );
        assert_eq!(
            observed["config"]["mcp_servers"]["tyde"]["command"],
            bridge.to_string_lossy().as_ref()
        );
        assert_eq!(
            observed["config"]["mcp_servers"]["tyde"]["args"],
            json!(["hermes-mcp-bridge"])
        );
        let servers = observed["descriptor"]["servers"]
            .as_array()
            .expect("descriptor servers");
        assert_eq!(servers.len(), 2);
        assert_eq!(servers[0]["name"], "local");
        assert_eq!(servers[0]["transport"]["kind"], "stdio");
        assert_eq!(servers[0]["transport"]["command"], "node");
        assert_eq!(servers[0]["transport"]["env"]["LOCAL_KEY"], "value");
        assert_eq!(servers[1]["name"], "shared");
        assert_eq!(servers[1]["transport"]["kind"], "http");
        assert_eq!(servers[1]["transport"]["url"], "https://tyde.invalid/mcp");
        assert_eq!(
            servers[1]["transport"]["headers"]["Authorization"],
            "Bearer private-token"
        );

        let persisted: Value = serde_json::from_slice(
            &fs::read(config_path).expect("read persisted fake Hermes config"),
        )
        .expect("parse persisted fake Hermes config");
        assert_eq!(persisted["model"]["default"], "native-model");
        assert_eq!(
            persisted["mcp_servers"]["native"]["command"],
            "native-command"
        );
        assert_eq!(
            persisted["mcp_servers"]["tyde"]["command"],
            bridge.to_string_lossy().as_ref()
        );

        backend.shutdown().await;
    }

    /// A modern Hermes with Tool Search deferral active hides MCP tools from
    /// the model-visible tools.show sections (they sit behind the tool_search
    /// bridge) and reports the managed toolset only via the tools.list
    /// registry view. Both startup gates must still pass — this pinned the
    /// live "Hermes did not register the managed Tyde MCP toolset" failure.
    #[tokio::test]
    async fn hermes_mcp_gates_accept_tool_search_deferred_toolset() {
        let _test_lock = TEST_HERMES_OVERRIDE_LOCK.lock().await;
        let dir = TempDir::new().expect("tempdir");
        let (fake, _observed_path, _config_path) = write_fake_deferred_mcp_gateway(&dir);
        let _python_guard = TestHermesPythonGuard::set(&fake);
        let bridge = dir.path().join("tyde-server");
        fs::write(&bridge, "bridge placeholder").expect("write bridge placeholder");
        let _bridge_guard = TestHermesBridgeExecutableGuard::set(&bridge.to_string_lossy());

        let startup_mcp_servers = vec![StartupMcpServer {
            name: "local".to_string(),
            transport: StartupMcpTransport::Stdio {
                command: "node".to_string(),
                args: vec!["server.js".to_string()],
                env: HashMap::new(),
            },
        }];
        let config = BackendSpawnConfig {
            startup_mcp_servers,
            ..BackendSpawnConfig::default()
        };
        let (backend, mut events) = HermesBackend::spawn(
            vec![dir.path().to_string_lossy().to_string()],
            config,
            payload("verify deferred MCP startup"),
        )
        .await
        .expect("spawn must succeed when the managed toolset is deferred behind tool_search");

        timeout(Duration::from_secs(2), async {
            while let Some(event) = events.recv().await {
                if matches!(event, ChatEvent::StreamEnd(_)) {
                    return;
                }
            }
            panic!("Hermes event stream closed before the deferred MCP test turn completed");
        })
        .await
        .expect("deferred MCP test turn should finish");

        backend.shutdown().await;
    }

    #[tokio::test]
    async fn register_runs_in_the_selected_profile_hermes_home() {
        // The registration script writes `mcp_servers.<managed>` into whatever
        // config.yaml its own HERMES_HOME resolves to. The gateway is later
        // spawned with the profile's HERMES_HOME, so if registration does not
        // inherit the same env it registers the bridge in the default profile,
        // the named profile's Hermes never launches the bridge, and startup
        // fails on the MCP-bridge readiness timeout.
        let _test_lock = TEST_HERMES_OVERRIDE_LOCK.lock().await;
        let dir = TempDir::new().expect("tempdir");
        let observed = dir.path().join("observed-home");
        let launcher = dir.path().join("fake_python_record_home.sh");
        fs::write(
            &launcher,
            format!(
                "#!/bin/sh\nprintf '%s' \"${{HERMES_HOME:-<unset>}}\" > {}\necho '[\"terminal\"]'\n",
                observed.display()
            ),
        )
        .expect("write fake python");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&launcher)
                .expect("launcher metadata")
                .permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&launcher, perms).expect("chmod launcher");
        }

        let profile_home = dir.path().join("profiles").join("work");
        let target = HermesSpawnTarget {
            program: launcher.to_string_lossy().to_string(),
            args: Vec::new(),
            env: HashMap::from([(
                crate::backend::hermes_config::HERMES_HOME_ENV.to_string(),
                profile_home.to_string_lossy().to_string(),
            )]),
            cwd: None,
            remote_host: None,
            display_program: "hermes".to_string(),
            provider_version: None,
        };

        let selected = register_hermes_mcp_bridge(&target, "/opt/tyde/tyde-server")
            .await
            .expect("registration must succeed");
        assert_eq!(selected, Some(vec!["terminal".to_string()]));
        assert_eq!(
            fs::read_to_string(&observed).expect("registration must have run"),
            profile_home.to_string_lossy(),
            "registration must run against the selected profile's HERMES_HOME"
        );
    }

    #[tokio::test]
    async fn register_reports_missing_hermes_mcp_extra() {
        // When Hermes is installed without the optional `mcp` package, the
        // registration script prints the marker and exits 0 instead of spawning
        // the bridge. Tyde must turn that into an actionable error at launch
        // rather than waiting out the startup timeout with an opaque message.
        let _test_lock = TEST_HERMES_OVERRIDE_LOCK.lock().await;
        let dir = TempDir::new().expect("tempdir");
        let launcher = dir.path().join("fake_python_mcp_missing.sh");
        fs::write(
            &launcher,
            format!("#!/bin/sh\necho {HERMES_MCP_MISSING_MARKER}\nexit 0\n"),
        )
        .expect("write fake python");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&launcher)
                .expect("launcher metadata")
                .permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&launcher, perms).expect("chmod launcher");
        }

        let target = HermesSpawnTarget {
            program: launcher.to_string_lossy().to_string(),
            args: Vec::new(),
            env: HashMap::new(),
            cwd: None,
            remote_host: None,
            display_program: "hermes".to_string(),
            provider_version: None,
        };

        let error = register_hermes_mcp_bridge(&target, "/opt/tyde/tyde-server")
            .await
            .expect_err("registration must fail when the Hermes mcp extra is absent");
        assert!(
            error.contains("mcp"),
            "error should name the missing package: {error}"
        );
        assert!(
            error.contains("pip install") && error.contains(".[mcp]"),
            "error should give the install command: {error}"
        );
        assert!(
            error.contains("hermes"),
            "error should reference the Hermes interpreter: {error}"
        );
    }

    #[tokio::test]
    async fn hermes_empty_allow_list_uses_only_empty_bridge() {
        let _test_lock = TEST_HERMES_OVERRIDE_LOCK.lock().await;
        let dir = TempDir::new().expect("tempdir");
        let (fake, observed_path, _) = write_fake_managed_mcp_gateway(&dir);
        let _python_guard = TestHermesPythonGuard::set(&fake);
        let bridge = dir.path().join("tyde-server");
        fs::write(&bridge, "bridge placeholder").expect("write bridge placeholder");
        let _bridge_guard = TestHermesBridgeExecutableGuard::set(&bridge.to_string_lossy());
        let mut config = BackendSpawnConfig::default();
        config.resolved_spawn_config.tool_policy =
            protocol::ToolPolicy::AllowList { tools: Vec::new() };

        let (backend, mut events) = HermesBackend::spawn(
            vec![dir.path().to_string_lossy().to_string()],
            config,
            payload("name this task"),
        )
        .await
        .expect("spawn tool-free fake Hermes backend");

        timeout(Duration::from_secs(2), async {
            while let Some(event) = events.recv().await {
                if matches!(event, ChatEvent::StreamEnd(_)) {
                    return;
                }
            }
            panic!("Hermes event stream closed before the tool-free turn completed");
        })
        .await
        .expect("tool-free Hermes turn should finish");

        let observed: Value = serde_json::from_slice(
            &fs::read(&observed_path).expect("read observed process config"),
        )
        .expect("parse observed process config");
        assert_eq!(observed["toolsets"], MANAGED_SERVER_NAME);
        assert_eq!(observed["descriptor"]["servers"], json!([]));

        backend.shutdown().await;
    }

    #[tokio::test]
    async fn hermes_probe_session_settings_schema_uses_model_options() {
        let _test_lock = TEST_HERMES_OVERRIDE_LOCK.lock().await;
        let dir = TempDir::new().expect("tempdir");
        let fake = write_fake_gateway(
            &dir,
            r#"
import json, sys
print(json.dumps({"jsonrpc":"2.0","method":"event","params":{"type":"gateway.ready","payload":{"skin":"default"}}}), flush=True)
for line in sys.stdin:
    req = json.loads(line)
    rid = req["id"]
    method = req["method"]
    if method == "model.options":
        print(json.dumps({"jsonrpc":"2.0","id":rid,"result":{
            "provider":"openrouter",
            "model":"anthropic/claude-haiku-4.5",
            "providers":[{
                "slug":"openrouter",
                "name":"OpenRouter",
                "authenticated":True,
                "models":["anthropic/claude-haiku-4.5"]
            }]
        }}), flush=True)
    else:
        print(json.dumps({"jsonrpc":"2.0","id":rid,"result":{}}), flush=True)
"#,
        );
        let _guard = TestHermesPythonGuard::set(&fake);
        let home = TempDir::new().expect("hermes home");
        let _home_guard = TestHermesHomeGuard::set(home.path());

        let probe = probe_session_settings_schema(
            &[dir.path().to_string_lossy().to_string()],
            &HashMap::new(),
        )
        .await
        .expect("schema");

        assert!(
            probe.schema.fields.iter().any(|field| field.key == "model"),
            "dynamic Hermes schema must include model options: {probe:?}"
        );
        // A lone default profile needs no profile picker.
        assert!(
            probe
                .schema
                .fields
                .iter()
                .all(|field| field.key != "profile"),
            "single-profile schema must not expose a profile field: {probe:?}"
        );
        assert_eq!(probe.profiles.len(), 1);
        assert_eq!(probe.profiles[0].name, "default");
        assert_eq!(
            probe.profiles[0].summary.as_deref(),
            Some("openrouter/anthropic/claude-haiku-4.5")
        );
    }

    #[tokio::test]
    async fn hermes_native_settings_snapshot_reports_profiles_and_providers() {
        let _test_lock = TEST_HERMES_OVERRIDE_LOCK.lock().await;
        let dir = TempDir::new().expect("tempdir");
        let fake = write_fake_gateway(
            &dir,
            r#"
import json, sys
print(json.dumps({"jsonrpc":"2.0","method":"event","params":{"type":"gateway.ready","payload":{"skin":"default"}}}), flush=True)
for line in sys.stdin:
    req = json.loads(line)
    rid = req["id"]
    method = req["method"]
    if method == "model.options":
        print(json.dumps({"jsonrpc":"2.0","id":rid,"result":{
            "provider":"openrouter",
            "model":"anthropic/claude-haiku-4.5",
            "providers":[
                {"slug":"openrouter","name":"OpenRouter","authenticated":True,
                 "auth_type":"api_key","key_env":"OPENROUTER_API_KEY",
                 "models":["anthropic/claude-haiku-4.5"]},
                {"slug":"anthropic","name":"Anthropic","authenticated":False,
                 "auth_type":"api_key","key_env":"ANTHROPIC_API_KEY",
                 "warning":"set ANTHROPIC_API_KEY","models":["claude-sonnet-5"]}
            ]
        }}), flush=True)
    else:
        print(json.dumps({"jsonrpc":"2.0","id":rid,"result":{}}), flush=True)
"#,
        );
        let _guard = TestHermesPythonGuard::set(&fake);
        let home = TempDir::new().expect("hermes home");
        fs::write(
            home.path().join("config.yaml"),
            "model:\n  provider: openrouter\n  default: anthropic/claude-haiku-4.5\n",
        )
        .expect("write config");
        fs::create_dir_all(home.path().join("profiles/grok")).expect("named profile");
        let _home_guard = TestHermesHomeGuard::set(home.path());

        let snapshot = native_settings_snapshot(&[dir.path().to_string_lossy().to_string()]).await;

        assert_eq!(snapshot.backend_kind, BackendKind::Hermes);
        assert_eq!(snapshot.status, BackendConfigSnapshotStatus::Ready);
        let doc: protocol::hermes_config::HermesNativeSettingsDoc =
            serde_json::from_value(snapshot.settings.expect("settings doc")).expect("typed doc");
        assert_eq!(doc.profiles.len(), 2);
        let default = &doc.profiles[0];
        assert_eq!(default.name, "default");
        assert_eq!(default.config.model.provider.as_deref(), Some("openrouter"));
        assert_eq!(
            default.config.model.model.as_deref(),
            Some("anthropic/claude-haiku-4.5")
        );
        assert_eq!(default.active_provider.as_deref(), Some("openrouter"));
        let providers = default.providers.as_ref().expect("provider states");
        assert_eq!(providers.len(), 2);
        assert!(providers[0].authenticated);
        assert!(!providers[1].authenticated);
        assert_eq!(providers[1].key_env.as_deref(), Some("ANTHROPIC_API_KEY"));
        assert_eq!(doc.profiles[1].name, "grok");
        assert!(doc.actions.is_empty());
    }

    #[test]
    fn hermes_multi_profile_schema_exposes_profile_select_and_per_profile_models() {
        let profiles = vec![
            HermesProfileRef {
                name: "default".to_string(),
                home_dir: PathBuf::from("/hermes-home"),
            },
            HermesProfileRef {
                name: "claude".to_string(),
                home_dir: PathBuf::from("/hermes-home/profiles/claude"),
            },
            HermesProfileRef {
                name: "gpt".to_string(),
                home_dir: PathBuf::from("/hermes-home/profiles/gpt"),
            },
        ];
        let default_payload = json!({
            "provider": "openrouter",
            "model": "minimax/minimax-m3",
            "providers": [{
                "slug": "openrouter", "name": "OpenRouter", "authenticated": true,
                "models": ["minimax/minimax-m3"]
            }]
        });
        let claude_payload = json!({
            "provider": "anthropic",
            "model": "claude-sonnet-5",
            "providers": [{
                "slug": "anthropic", "name": "Anthropic", "authenticated": true,
                "models": ["claude-sonnet-5"]
            }]
        });
        let probe = session_schema_probe_from_model_options(
            &profiles,
            vec![
                Ok(default_payload),
                Ok(claude_payload),
                Err("gateway exploded".to_string()),
            ],
            &HashMap::new(),
        )
        .expect("probe");

        let profile_field = probe.schema.fields.first().expect("profile field first");
        assert_eq!(profile_field.key, "profile");
        match &profile_field.field_type {
            SessionSettingFieldType::Select {
                options,
                default,
                nullable,
            } => {
                let values: Vec<&str> = options.iter().map(|o| o.value.as_str()).collect();
                assert_eq!(values, vec!["default", "claude", "gpt"]);
                assert_eq!(options[0].label, "Default");
                assert!(
                    options[2].label.contains("Unavailable")
                        && options[2].label.contains("gateway exploded"),
                    "failed profiles must remain visible with their error: {:?}",
                    options[2]
                );
                assert_eq!(default.as_deref(), Some("default"));
                assert!(!nullable);
            }
            other => panic!("profile must be Select, got {other:?}"),
        }

        let model_field = probe
            .schema
            .fields
            .iter()
            .find(|field| field.key == "model")
            .expect("model field");
        let by_profile = model_field
            .select_options_by_setting
            .as_ref()
            .expect("per-profile model options");
        assert_eq!(by_profile.setting_key, "profile");
        assert_eq!(by_profile.values.len(), 2);
        assert_eq!(by_profile.values[1].setting_value, "claude");
        assert_eq!(
            by_profile.values[1].options[0].value,
            encode_model_option_value("claude-sonnet-5", Some("anthropic"))
        );

        assert_eq!(probe.profiles.len(), 3);
        assert_eq!(
            probe.profiles[1].summary.as_deref(),
            Some("anthropic/claude-sonnet-5")
        );
        let broken = &probe.profiles[2];
        assert_eq!(broken.name, "gpt");
        assert!(
            broken
                .error
                .as_deref()
                .is_some_and(|error| error.contains("gateway exploded")),
            "broken profile must carry its probe error: {broken:?}"
        );
    }

    #[test]
    fn hermes_disabled_providers_are_dropped_from_model_options() {
        let payload = json!({
            "provider": "bedrock",
            "model": "claude-fable-5",
            "providers": [
                {
                    "slug": "copilot", "name": "GitHub Copilot", "authenticated": true,
                    "models": ["gpt-5.5", "claude-opus-4.8"]
                },
                {
                    "slug": "bedrock", "name": "AWS Bedrock", "authenticated": true,
                    "models": ["claude-fable-5"]
                }
            ]
        });
        let profiles = vec![HermesProfileRef {
            name: protocol::hermes_config::HERMES_DEFAULT_PROFILE.to_string(),
            home_dir: PathBuf::from("/hermes-home"),
        }];
        let disabled = HashMap::from([("default".to_string(), vec!["copilot".to_string()])]);

        let probe =
            session_schema_probe_from_model_options(&profiles, vec![Ok(payload)], &disabled)
                .expect("probe");
        let model_field = probe
            .schema
            .fields
            .iter()
            .find(|field| field.key == "model")
            .expect("model field");
        let SessionSettingFieldType::Select { options, .. } = &model_field.field_type else {
            panic!("model field must be a select: {model_field:?}");
        };
        let values: Vec<&str> = options.iter().map(|o| o.value.as_str()).collect();
        assert!(
            values.iter().all(|value| !value.contains("copilot")),
            "a disabled provider's models must not be offered: {values:?}"
        );
        assert!(
            values.iter().any(|value| value.contains("bedrock")),
            "an enabled provider must still be offered: {values:?}"
        );
    }

    #[test]
    fn hermes_disabling_every_provider_says_so_instead_of_blaming_hermes() {
        let payload = json!({
            "provider": "bedrock",
            "model": "claude-fable-5",
            "providers": [{
                "slug": "bedrock", "name": "AWS Bedrock", "authenticated": true,
                "models": ["claude-fable-5"]
            }]
        });
        let profiles = vec![HermesProfileRef {
            name: protocol::hermes_config::HERMES_DEFAULT_PROFILE.to_string(),
            home_dir: PathBuf::from("/hermes-home"),
        }];
        let disabled = HashMap::from([("default".to_string(), vec!["bedrock".to_string()])]);

        let error =
            session_schema_probe_from_model_options(&profiles, vec![Ok(payload)], &disabled)
                .expect_err("no selectable models must fail the schema");
        assert!(
            error.contains("disabled in Tyde"),
            "the user disabled these, so the message must point at Tyde's own list: {error}"
        );
        assert!(error.contains("bedrock"), "{error}");
    }

    #[test]
    fn hermes_schema_fails_when_default_profile_probe_fails() {
        let profiles = vec![HermesProfileRef {
            name: "default".to_string(),
            home_dir: PathBuf::from("/hermes-home"),
        }];
        let error = session_schema_probe_from_model_options(
            &profiles,
            vec![Err("no authenticated providers".to_string())],
            &HashMap::new(),
        )
        .expect_err("default profile failure must fail the schema");
        assert!(error.contains("no authenticated providers"), "{error}");
    }

    #[tokio::test]
    async fn hermes_persist_native_settings_runs_actions_and_writes_config() {
        let _test_lock = TEST_HERMES_OVERRIDE_LOCK.lock().await;
        let dir = TempDir::new().expect("tempdir");
        let rpc_log = dir.path().join("rpc.jsonl");
        let fake = write_fake_gateway(
            &dir,
            &format!(
                r#"
import json, sys
print(json.dumps({{"jsonrpc":"2.0","method":"event","params":{{"type":"gateway.ready","payload":{{"skin":"default"}}}}}}), flush=True)
for line in sys.stdin:
    req = json.loads(line)
    rid = req["id"]
    method = req["method"]
    with open({rpc_log:?}, "a") as f:
        f.write(json.dumps({{"method": method, "params": req.get("params")}}) + "\n")
    if method == "model.save_key":
        print(json.dumps({{"jsonrpc":"2.0","id":rid,"result":{{"slug":req["params"]["slug"],"authenticated":True}}}}), flush=True)
    elif method == "model.disconnect":
        print(json.dumps({{"jsonrpc":"2.0","id":rid,"result":{{"disconnected":True}}}}), flush=True)
    else:
        print(json.dumps({{"jsonrpc":"2.0","id":rid,"result":{{}}}}), flush=True)
"#
            ),
        );
        let _guard = TestHermesPythonGuard::set(&fake);
        let home = TempDir::new().expect("hermes home");
        fs::write(
            home.path().join("config.yaml"),
            "model:\n  provider: openrouter\n  default: minimax/minimax-m3\ntoolsets:\n  - hermes-cli\n",
        )
        .expect("write config");
        let _home_guard = TestHermesHomeGuard::set(home.path());

        let mut doc = native_settings_doc(&[dir.path().to_string_lossy().to_string()])
            .await
            .expect("snapshot doc");
        doc.profiles[0].base_config = Some(doc.profiles[0].config.clone());
        doc.profiles[0].config.model.provider = Some("anthropic".to_string());
        doc.profiles[0].config.model.model = Some("claude-sonnet-5".to_string());
        doc.actions = vec![
            protocol::hermes_config::HermesCredentialAction::SaveApiKey {
                profile: "default".to_string(),
                provider: "anthropic".to_string(),
                api_key: "sk-test-value".to_string(),
            },
            protocol::hermes_config::HermesCredentialAction::Disconnect {
                profile: "default".to_string(),
                provider: "copilot".to_string(),
            },
        ];

        persist_native_settings(
            serde_json::to_value(&doc).expect("doc to value"),
            &[dir.path().to_string_lossy().to_string()],
        )
        .await
        .expect("persist");

        let rpc = fs::read_to_string(&rpc_log).expect("rpc log");
        assert!(
            rpc.contains("model.save_key") && rpc.contains("\"slug\": \"anthropic\""),
            "save_key must reach the gateway: {rpc}"
        );
        assert!(rpc.contains("model.disconnect"), "{rpc}");

        let config = fs::read_to_string(home.path().join("config.yaml")).expect("config");
        assert!(config.contains("provider: anthropic"), "{config}");
        assert!(config.contains("default: claude-sonnet-5"), "{config}");
        assert!(
            config.contains("hermes-cli"),
            "unmodeled keys preserved: {config}"
        );
    }

    #[tokio::test]
    async fn hermes_named_profile_disconnect_is_blocked_without_blocking_config_save() {
        let _test_lock = TEST_HERMES_OVERRIDE_LOCK.lock().await;
        let dir = TempDir::new().expect("tempdir");
        let rpc_log = dir.path().join("rpc.jsonl");
        let spawn_log = dir.path().join("spawned");
        let fake = write_fake_gateway(
            &dir,
            &format!(
                r#"
import json, sys
with open({spawn_log:?}, "w") as output:
    output.write("spawned")
print(json.dumps({{"jsonrpc":"2.0","method":"event","params":{{"type":"gateway.ready","payload":{{}}}}}}), flush=True)
for line in sys.stdin:
    req = json.loads(line)
    with open({rpc_log:?}, "a") as output:
        output.write(json.dumps({{"method": req["method"], "params": req.get("params")}}) + "\n")
    print(json.dumps({{"jsonrpc":"2.0","id":req["id"],"result":{{}}}}), flush=True)
"#
            ),
        );
        let _python_guard = TestHermesPythonGuard::set(&fake);
        let home = TempDir::new().expect("Hermes home");
        let profile_home = home.path().join("profiles").join("work");
        fs::create_dir_all(&profile_home).expect("profile home");
        fs::write(
            profile_home.join("config.yaml"),
            "model:\n  provider: openrouter\n  default: old/model\n",
        )
        .expect("profile config");
        let root_auth = home.path().join("auth.json");
        let root_auth_bytes = br#"{"providers":{"copilot":{"token":"synthetic"}}}"#;
        fs::write(&root_auth, root_auth_bytes).expect("synthetic root auth");
        let _home_guard = TestHermesHomeGuard::set(home.path());

        let base = hermes_config::load_profile_config(&profile_home).expect("base config");
        let mut changed = base.clone();
        changed.model.model = Some("new/model".to_string());
        let doc = protocol::hermes_config::HermesNativeSettingsDoc {
            version: protocol::hermes_config::HERMES_NATIVE_SETTINGS_VERSION,
            profile_actions: Vec::new(),
            profiles: vec![protocol::hermes_config::HermesProfileSettings {
                name: "work".to_string(),
                home_dir: profile_home.to_string_lossy().to_string(),
                config: changed,
                base_config: Some(base),
                providers: None,
                providers_error: None,
                active_model: None,
                active_provider: None,
                toolsets: None,
            }],
            actions: vec![
                protocol::hermes_config::HermesCredentialAction::Disconnect {
                    profile: "work".to_string(),
                    provider: "copilot".to_string(),
                },
            ],
        };

        let outcome = persist_native_settings(
            serde_json::to_value(doc).expect("settings"),
            &[dir.path().to_string_lossy().to_string()],
        )
        .await
        .expect("unrelated configuration writes must complete");
        let error = outcome
            .partial_error_message()
            .expect("blocked credential action must be reported");

        assert!(
            error.contains("saved the unrelated configuration"),
            "{error}"
        );
        assert!(error.contains("cannot prove"), "{error}");
        assert_eq!(
            fs::read(&root_auth).expect("root auth"),
            root_auth_bytes,
            "named-profile action must never mutate the default credential store"
        );
        assert!(
            fs::read_to_string(profile_home.join("config.yaml"))
                .expect("saved profile config")
                .contains("new/model"),
            "unrelated config edit must still land"
        );
        let rpc = fs::read_to_string(&rpc_log).unwrap_or_default();
        assert!(
            !rpc.contains("model.disconnect"),
            "unsafe disconnect RPC must not be sent: {rpc}"
        );
        assert!(
            !spawn_log.exists(),
            "a named-profile disconnect rejected locally must not spawn Hermes"
        );
    }

    #[tokio::test]
    async fn hermes_persist_skips_rewrite_for_unchanged_profiles() {
        let _test_lock = TEST_HERMES_OVERRIDE_LOCK.lock().await;
        let home = TempDir::new().expect("hermes home");
        let original = "# hand-written comment\nmodel:\n  provider: openrouter\n  default: minimax/minimax-m3\n";
        fs::write(home.path().join("config.yaml"), original).expect("write config");
        let _home_guard = TestHermesHomeGuard::set(home.path());

        // An unchanged config section must not rewrite the file (which would
        // drop comments); build the doc directly from disk without a gateway.
        let doc = protocol::hermes_config::HermesNativeSettingsDoc {
            version: protocol::hermes_config::HERMES_NATIVE_SETTINGS_VERSION,
            profile_actions: Vec::new(),
            profiles: vec![protocol::hermes_config::HermesProfileSettings {
                name: "default".to_string(),
                home_dir: home.path().to_string_lossy().to_string(),
                config: hermes_config::load_profile_config(home.path()).expect("load"),
                base_config: None,
                providers: None,
                providers_error: None,
                active_model: None,
                active_provider: None,
                toolsets: None,
            }],
            actions: Vec::new(),
        };
        persist_native_settings(serde_json::to_value(&doc).expect("doc"), &[])
            .await
            .expect("persist");

        let after = fs::read_to_string(home.path().join("config.yaml")).expect("config");
        assert_eq!(after, original, "unchanged profile must not be rewritten");
    }

    /// A save carrying profile actions applies them before anything else, so
    /// the rest of that same save sees the resulting set of profiles.
    #[tokio::test]
    async fn hermes_persist_applies_profile_actions_before_config_writes() {
        let _test_lock = TEST_HERMES_OVERRIDE_LOCK.lock().await;
        let home = TempDir::new().expect("hermes home");
        fs::write(
            home.path().join("config.yaml"),
            "model:\n  provider: bedrock\n",
        )
        .expect("write config");
        // A profile with history, to prove a delete takes the whole home.
        let doomed = home.path().join("profiles/doomed");
        fs::create_dir_all(doomed.join("sessions")).expect("profile dir");
        fs::write(doomed.join("state.db"), "sqlite").expect("state");
        let _home_guard = TestHermesHomeGuard::set(home.path());

        let mut doc = protocol::hermes_config::HermesNativeSettingsDoc {
            version: protocol::hermes_config::HERMES_NATIVE_SETTINGS_VERSION,
            profile_actions: vec![
                protocol::hermes_config::HermesProfileAction::CreateProfile {
                    name: "fresh".to_string(),
                    copy_config_from: None,
                },
                protocol::hermes_config::HermesProfileAction::DeleteProfile {
                    name: "doomed".to_string(),
                },
            ],
            profiles: Vec::new(),
            actions: Vec::new(),
        };
        // The doomed profile is still in the client's document, exactly as a
        // real save would carry it — it was on screen when the user hit
        // delete. Its config section must be skipped, not resolved.
        doc.profiles
            .push(protocol::hermes_config::HermesProfileSettings {
                name: "doomed".to_string(),
                home_dir: doomed.to_string_lossy().to_string(),
                config: protocol::hermes_config::HermesProfileConfig::default(),
                base_config: Some(protocol::hermes_config::HermesProfileConfig::default()),
                providers: None,
                providers_error: None,
                active_model: None,
                active_provider: None,
                toolsets: None,
            });

        persist_native_settings(serde_json::to_value(&doc).expect("doc"), &[])
            .await
            .expect("persist");

        assert!(!doomed.exists(), "a deleted profile's whole home must go");
        let fresh = home.path().join("profiles/fresh");
        assert!(fresh.is_dir(), "the created profile must exist");
        assert_eq!(
            fs::read_to_string(fresh.join("config.yaml")).expect("copied config"),
            "model:\n  provider: bedrock\n",
            "a new profile starts from the source profile's config"
        );

        let names: Vec<String> = hermes_config::discover_profiles_in(home.path())
            .expect("discover")
            .into_iter()
            .map(|profile| profile.name)
            .collect();
        assert_eq!(names, vec!["default".to_string(), "fresh".to_string()]);
    }

    /// Credentials are applied against a profile by name. Letting a save both
    /// delete a profile and key it would run one of those against a directory
    /// the other just removed, so the pair is refused outright.
    #[tokio::test]
    async fn hermes_persist_refuses_crediting_a_profile_the_same_save_deletes() {
        let _test_lock = TEST_HERMES_OVERRIDE_LOCK.lock().await;
        let home = TempDir::new().expect("hermes home");
        let profile = home.path().join("profiles/work");
        fs::create_dir_all(&profile).expect("profile dir");
        let _home_guard = TestHermesHomeGuard::set(home.path());

        let doc = protocol::hermes_config::HermesNativeSettingsDoc {
            version: protocol::hermes_config::HERMES_NATIVE_SETTINGS_VERSION,
            profile_actions: vec![
                protocol::hermes_config::HermesProfileAction::DeleteProfile {
                    name: "work".to_string(),
                },
            ],
            profiles: Vec::new(),
            actions: vec![
                protocol::hermes_config::HermesCredentialAction::SaveApiKey {
                    profile: "work".to_string(),
                    provider: "openrouter".to_string(),
                    api_key: "sk-test".to_string(),
                },
            ],
        };

        let error = persist_native_settings(serde_json::to_value(&doc).expect("doc"), &[])
            .await
            .expect_err("a delete + credential save must be refused");
        assert!(error.contains("same save that deletes it"), "{error}");
        assert!(
            profile.is_dir(),
            "a refused save must not have deleted anything"
        );
    }

    fn default_profile_doc(
        home: &Path,
        config: protocol::hermes_config::HermesProfileConfig,
        base_config: Option<protocol::hermes_config::HermesProfileConfig>,
    ) -> protocol::hermes_config::HermesNativeSettingsDoc {
        protocol::hermes_config::HermesNativeSettingsDoc {
            version: protocol::hermes_config::HERMES_NATIVE_SETTINGS_VERSION,
            profile_actions: Vec::new(),
            profiles: vec![protocol::hermes_config::HermesProfileSettings {
                name: "default".to_string(),
                home_dir: home.to_string_lossy().to_string(),
                config,
                base_config,
                providers: None,
                providers_error: None,
                active_model: None,
                active_provider: None,
                toolsets: None,
            }],
            actions: Vec::new(),
        }
    }

    #[tokio::test]
    async fn hermes_persist_refuses_stale_base_and_half_filled_fallbacks() {
        let _test_lock = TEST_HERMES_OVERRIDE_LOCK.lock().await;
        let home = TempDir::new().expect("hermes home");
        fs::write(
            home.path().join("config.yaml"),
            "model:\n  provider: openrouter\n  default: minimax/minimax-m3\n",
        )
        .expect("write config");
        let _home_guard = TestHermesHomeGuard::set(home.path());

        // Stale base: the disk changed after this snapshot was taken.
        let stale_base = protocol::hermes_config::HermesProfileConfig::default();
        let mut edited = stale_base.clone();
        edited.agent.max_turns = Some(50);
        let doc = default_profile_doc(home.path(), edited, Some(stale_base));
        let error = persist_native_settings(serde_json::to_value(&doc).expect("doc"), &[])
            .await
            .expect_err("stale base must be refused");
        assert!(error.contains("changed since it was loaded"), "{error}");
        let raw = fs::read_to_string(home.path().join("config.yaml")).expect("config");
        assert!(
            !raw.contains("max_turns"),
            "refused save must not touch the file: {raw}"
        );

        // Half-filled fallback: rejected before anything is written.
        let mut invalid = hermes_config::load_profile_config(home.path()).expect("load");
        invalid
            .fallback_providers
            .push(protocol::hermes_config::HermesFallbackProvider {
                provider: "anthropic".to_string(),
                model: String::new(),
                extra: Default::default(),
            });
        let doc = default_profile_doc(home.path(), invalid, None);
        let error = persist_native_settings(serde_json::to_value(&doc).expect("doc"), &[])
            .await
            .expect_err("half-filled fallback must be refused");
        assert!(error.contains("provider and a model"), "{error}");
        let raw = fs::read_to_string(home.path().join("config.yaml")).expect("config");
        assert!(!raw.contains("fallback_providers"), "{raw}");
    }

    #[tokio::test]
    async fn hermes_empty_root_gateway_runs_from_tyde_no_root_cwd() {
        let _test_lock = TEST_HERMES_OVERRIDE_LOCK.lock().await;
        let dir = TempDir::new().expect("tempdir");
        let cwd_log = dir.path().join("cwd.txt");
        let fake = write_fake_gateway(
            &dir,
            &format!(
                r#"
import json, os, sys
with open({cwd_log:?}, "w") as f:
    f.write(os.getcwd())
print(json.dumps({{"jsonrpc":"2.0","method":"event","params":{{"type":"gateway.ready","payload":{{"skin":"default"}}}}}}), flush=True)
for line in sys.stdin:
    req = json.loads(line)
    rid = req["id"]
    method = req["method"]
    params = req.get("params") or {{}}
    if method == "session.create":
        print(json.dumps({{"jsonrpc":"2.0","id":rid,"result":{{"session_id":"live1","stored_session_id":"stored1","messages":[],"info":{{}}}}}}), flush=True)
    elif method == "prompt.submit":
        sid = params["session_id"]
        print(json.dumps({{"jsonrpc":"2.0","id":rid,"result":{{"status":"streaming"}}}}), flush=True)
        print(json.dumps({{"jsonrpc":"2.0","method":"event","params":{{"type":"message.start","session_id":sid}}}}), flush=True)
        print(json.dumps({{"jsonrpc":"2.0","method":"event","params":{{"type":"message.complete","session_id":sid,"payload":{{"text":"ok","status":"complete"}}}}}}), flush=True)
    elif method == "session.usage":
        print(json.dumps({{"jsonrpc":"2.0","id":rid,"result":{{"input":0,"output":0,"total":0}}}}), flush=True)
    else:
        print(json.dumps({{"jsonrpc":"2.0","id":rid,"result":{{}}}}), flush=True)
"#,
                cwd_log = cwd_log.to_string_lossy()
            ),
        );
        let _guard = TestHermesPythonGuard::set(&fake);
        let (backend, mut events) =
            HermesBackend::spawn(Vec::new(), BackendSpawnConfig::default(), payload("hello"))
                .await
                .expect("spawn fake hermes");
        let mut warnings = Vec::new();
        timeout(Duration::from_secs(2), async {
            while let Some(event) = events.recv().await {
                match event {
                    ChatEvent::MessageAdded(ChatMessage {
                        sender: MessageSender::Warning,
                        content,
                        ..
                    }) => warnings.push(content),
                    ChatEvent::StreamEnd(_) => break,
                    _ => {}
                }
            }
        })
        .await
        .expect("turn should finish");
        backend.shutdown().await;
        assert!(
            warnings.is_empty(),
            "optional usage diagnostics must stay out of chat: {warnings:?}"
        );

        let cwd = fs::read_to_string(&cwd_log).expect("read cwd log");
        assert!(
            cwd.ends_with(".tyde/hermes/no-root"),
            "empty-root gateway cwd must be Tyde-owned no-root dir, got {cwd}"
        );
    }

    #[tokio::test]
    async fn hermes_shutdown_waits_for_child_eof_and_exit() {
        let _test_lock = TEST_HERMES_OVERRIDE_LOCK.lock().await;
        let dir = TempDir::new().expect("tempdir");
        let exited = dir.path().join("child-exited");
        let fake = write_fake_gateway(
            &dir,
            &format!(
                r#"
import json, sys
print(json.dumps({{"jsonrpc":"2.0","method":"event","params":{{"type":"gateway.ready","payload":{{}}}}}}), flush=True)
for line in sys.stdin:
    req = json.loads(line)
    method = req["method"]
    if method == "session.create":
        result = {{"session_id":"live","stored_session_id":"stored"}}
    elif method == "prompt.submit":
        result = {{"status":"streaming"}}
    else:
        result = {{}}
    print(json.dumps({{"jsonrpc":"2.0","id":req["id"],"result":result}}), flush=True)
    if method == "prompt.submit":
        print(json.dumps({{"jsonrpc":"2.0","method":"event","params":{{"type":"message.start","session_id":"live"}}}}), flush=True)
        print(json.dumps({{"jsonrpc":"2.0","method":"event","params":{{"type":"message.complete","session_id":"live","payload":{{"text":"done","status":"complete","usage":{{"input":1,"output":1,"total":2}}}}}}}}), flush=True)
# Reaching here proves stdin EOF without racing the test-only force-kill grace.
print("final shutdown diagnostic", file=sys.stderr, flush=True)
with open({exited:?}, "w") as output:
    output.write("exited")
"#
            ),
        );
        let _guard = TestHermesPythonGuard::set(&fake);
        let (backend, mut events) =
            HermesBackend::spawn(Vec::new(), BackendSpawnConfig::default(), payload("hello"))
                .await
                .expect("spawn fake Hermes");
        timeout(Duration::from_secs(2), async {
            while let Some(event) = events.recv().await {
                if matches!(event, ChatEvent::StreamEnd(_)) {
                    return;
                }
            }
            panic!("event stream closed before completion");
        })
        .await
        .expect("turn");

        backend.shutdown().await;

        assert_eq!(
            fs::read_to_string(&exited).expect("child exit marker"),
            "exited",
            "shutdown must await child EOF/exit before returning"
        );
    }

    #[tokio::test]
    async fn hermes_owner_loss_retires_background_command_once() {
        let _test_lock = TEST_HERMES_OVERRIDE_LOCK.lock().await;
        let dir = TempDir::new().expect("tempdir");
        let fake = write_fake_gateway(
            &dir,
            r#"
import json, sys
print(json.dumps({"jsonrpc":"2.0","method":"event","params":{"type":"gateway.ready","payload":{}}}), flush=True)
for line in sys.stdin:
    req = json.loads(line)
    method = req["method"]
    if method == "session.create":
        result = {"session_id":"live","stored_session_id":"stored"}
    elif method == "prompt.submit":
        result = {"status":"streaming"}
    elif method == "session.usage":
        result = {"input":1,"output":1,"total":2}
    elif method == "session.context_breakdown":
        result = {"context_used":2,"context_max":200000}
    else:
        result = {}
    print(json.dumps({"jsonrpc":"2.0","id":req["id"],"result":result}), flush=True)
    if method == "prompt.submit":
        def emit(event_type, payload=None):
            params = {"type":event_type,"session_id":"live"}
            if payload is not None:
                params["payload"] = payload
            print(json.dumps({"jsonrpc":"2.0","method":"event","params":params}), flush=True)
        emit("message.start")
        emit("tool.start", {
            "tool_id":"terminal-1",
            "name":"terminal",
            "args":{"command":"sleep 30","background":True}
        })
        emit("tool.complete", {
            "tool_id":"terminal-1",
            "name":"terminal",
            "result":{"session_id":"proc-1","exit_code":0}
        })
        emit("message.complete", {
            "text":"launched",
            "status":"complete",
            "usage":{"input":1,"output":1,"total":2}
        })
"#,
        );
        let _guard = TestHermesPythonGuard::set(&fake);
        let (backend, mut events) =
            HermesBackend::spawn(Vec::new(), BackendSpawnConfig::default(), payload("launch"))
                .await
                .expect("spawn fake Hermes");
        let mut observed = Vec::new();
        timeout(Duration::from_secs(2), async {
            while let Some(event) = events.recv().await {
                let turn_done = matches!(event, ChatEvent::StreamEnd(_));
                observed.push(event);
                if turn_done {
                    return;
                }
            }
            panic!("event stream closed before launch completed");
        })
        .await
        .expect("background launch");

        backend.shutdown().await;
        timeout(Duration::from_secs(2), async {
            while let Some(event) = events.recv().await {
                observed.push(event);
            }
        })
        .await
        .expect("Hermes teardown");

        assert_eq!(
            observed
                .iter()
                .filter(|event| matches!(
                    event,
                    ChatEvent::ToolProgress(ToolProgressData {
                        tool_call_id,
                        update: ToolProgressUpdate::BackgroundTask(BackgroundTaskState {
                            task_id,
                            status: BackgroundTaskStatus::Stopped,
                            ..
                        }),
                        ..
                    }) if tool_call_id == "terminal-1" && task_id == "proc-1"
                ))
                .count(),
            1,
            "irreversible Hermes owner loss must retire Running exactly once"
        );
        assert_eq!(
            observed
                .iter()
                .filter(|event| matches!(
                    event,
                    ChatEvent::ToolExecutionCompleted(ToolExecutionCompletedData {
                        tool_call_id,
                        ..
                    }) if tool_call_id == "terminal-1"
                ))
                .count(),
            1,
            "the launch completion remains authoritative; teardown must not duplicate it"
        );
    }

    #[tokio::test]
    async fn hermes_shutdown_forces_a_child_that_ignores_eof() {
        let _test_lock = TEST_HERMES_OVERRIDE_LOCK.lock().await;
        let dir = TempDir::new().expect("tempdir");
        let survived = dir.path().join("child-survived");
        let fake = write_fake_gateway(
            &dir,
            &format!(
                r#"
import json, sys, time
print(json.dumps({{"jsonrpc":"2.0","method":"event","params":{{"type":"gateway.ready","payload":{{}}}}}}), flush=True)
for line in sys.stdin:
    req = json.loads(line)
    method = req["method"]
    if method == "session.create":
        result = {{"session_id":"live","stored_session_id":"stored"}}
    elif method == "prompt.submit":
        result = {{"status":"streaming"}}
    else:
        result = {{}}
    print(json.dumps({{"jsonrpc":"2.0","id":req["id"],"result":result}}), flush=True)
    if method == "prompt.submit":
        print(json.dumps({{"jsonrpc":"2.0","method":"event","params":{{"type":"message.start","session_id":"live"}}}}), flush=True)
        print(json.dumps({{"jsonrpc":"2.0","method":"event","params":{{"type":"message.complete","session_id":"live","payload":{{"text":"done","status":"complete","usage":{{"input":1,"output":1,"total":2}}}}}}}}), flush=True)
time.sleep(0.5)
with open({survived:?}, "w") as output:
    output.write("survived")
"#
            ),
        );
        let _guard = TestHermesPythonGuard::set(&fake);
        let (backend, mut events) =
            HermesBackend::spawn(Vec::new(), BackendSpawnConfig::default(), payload("hello"))
                .await
                .expect("spawn fake Hermes");
        timeout(Duration::from_secs(2), async {
            while let Some(event) = events.recv().await {
                if matches!(event, ChatEvent::StreamEnd(_)) {
                    return;
                }
            }
            panic!("event stream closed before completion");
        })
        .await
        .expect("turn");

        backend.shutdown().await;
        tokio::time::sleep(Duration::from_millis(600)).await;

        assert!(
            !survived.exists(),
            "shutdown must force a child that remains alive after stdin EOF"
        );
    }

    #[tokio::test]
    async fn hermes_child_waiter_awaits_and_reports_the_real_exit_code() {
        let dir = TempDir::new().expect("tempdir");
        let fake = write_fake_gateway(&dir, "raise SystemExit(23)\n");
        let mut command = Command::new(fake);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = command.group_spawn().expect("spawn child");
        let (inbound_tx, mut inbound_rx) = mpsc::unbounded_channel();
        let (_force_tx, force_rx) = mpsc::unbounded_channel();

        spawn_child_waiter(child, inbound_tx, force_rx);

        let event = timeout(Duration::from_secs(2), inbound_rx.recv())
            .await
            .expect("child wait")
            .expect("closed event");
        assert!(matches!(event, HermesGatewayInbound::Closed(Some(23))));
    }

    #[test]
    fn hermes_read_only_instructions_use_the_gateway_system_overlay() {
        let resolved = ResolvedSpawnConfig {
            access_mode: BackendAccessMode::ReadOnly,
            ..ResolvedSpawnConfig::default()
        };
        let params = build_session_create_params(&[], &SessionSettingsValues::default(), None)
            .expect("params");
        let cwd = params["cwd"].as_str().expect("cwd");
        assert!(
            cwd.ends_with(".tyde/hermes/no-root"),
            "empty-root Hermes sessions must use Tyde-owned no-root cwd, got {cwd}"
        );
        let ambient_cwd = std::env::current_dir()
            .ok()
            .map(|path| path.to_string_lossy().to_string());
        assert_ne!(
            Some(cwd),
            ambient_cwd.as_deref(),
            "empty-root Hermes sessions must not fall back to ambient cwd"
        );
        assert!(
            params.get("messages").is_none(),
            "Tyde instructions must not be persisted as ordinary history"
        );
        let instructions = render_hermes_spawn_instructions(&resolved, true).expect("instructions");
        assert!(instructions.contains("Backend access mode is read-only"));
        assert!(HERMES_MCP_GATEWAY_ENTRY.contains("ephemeral_system_prompt"));
        assert!(HERMES_MCP_GATEWAY_ENTRY.contains(TYDE_HERMES_SYSTEM_PROMPT_ENV));
        let fallback = build_session_create_params(
            &[],
            &SessionSettingsValues::default(),
            Some(&instructions),
        )
        .expect("remote fallback params");
        assert_eq!(
            fallback["messages"][0]["content"].as_str(),
            Some(instructions.as_str())
        );
    }

    #[test]
    fn hermes_skill_seed_is_a_compact_progressive_catalog() {
        let body_sentinel = "BODY_SENTINEL_SHOULD_NOT_BE_EAGERLY_INJECTED";
        let resolved = ResolvedSpawnConfig {
            skills: vec![
                crate::agent::customization::ResolvedSkill::test_fixture(
                    "Review changes",
                    &format!("{body_sentinel}\n{}", "x".repeat(20_000)),
                ),
                crate::agent::customization::ResolvedSkill::test_fixture(
                    "Trace failures",
                    "another private body",
                ),
            ],
            ..ResolvedSpawnConfig::default()
        };

        let seed = render_hermes_spawn_instructions(&resolved, true).expect("system overlay");

        assert!(seed.contains("Review changes"));
        assert!(seed.contains("Trace failures"));
        assert!(seed.contains("skill discovery"));
        assert!(!seed.contains(body_sentinel));
        assert!(!seed.contains("another private body"));
        assert!(
            seed.len() < 1_000,
            "skill catalogue must stay bounded independently of body size"
        );
    }

    #[tokio::test]
    async fn hermes_resume_reseeds_tyde_instructions_outside_history() {
        let _test_lock = TEST_HERMES_OVERRIDE_LOCK.lock().await;
        let dir = TempDir::new().expect("tempdir");
        let prompt_log = dir.path().join("prompt.txt");
        let fake = write_fake_gateway(
            &dir,
            &format!(
                r#"
import json, os, sys
with open({prompt_log:?}, "w") as output:
    output.write(os.environ.get("TYDE_HERMES_SYSTEM_PROMPT", ""))
print(json.dumps({{"jsonrpc":"2.0","method":"event","params":{{"type":"gateway.ready","payload":{{}}}}}}), flush=True)
for line in sys.stdin:
    req = json.loads(line)
    method = req["method"]
    if method == "session.resume":
        result = {{"session_id":"live","resumed":"stored"}}
    elif method == "session.history":
        result = {{"messages":[]}}
    else:
        result = {{}}
    print(json.dumps({{"jsonrpc":"2.0","id":req["id"],"result":result}}), flush=True)
"#
            ),
        );
        let _guard = TestHermesPythonGuard::set(&fake);
        let mut config = BackendSpawnConfig::default();
        config.resolved_spawn_config.access_mode = BackendAccessMode::ReadOnly;
        config.resolved_spawn_config.skills =
            vec![crate::agent::customization::ResolvedSkill::test_fixture(
                "Review changes",
                "PRIVATE_SKILL_BODY",
            )];

        let (backend, _events) =
            HermesBackend::resume(Vec::new(), config, SessionId("stored".to_string()))
                .await
                .expect("resume with a Tyde system overlay");
        backend.shutdown().await;

        let prompt = fs::read_to_string(prompt_log).expect("resume prompt");
        assert!(
            prompt.contains("Backend access mode is read-only"),
            "{prompt}"
        );
        assert!(prompt.contains("Review changes"), "{prompt}");
        assert!(!prompt.contains("PRIVATE_SKILL_BODY"), "{prompt}");
    }

    #[test]
    fn hermes_runtime_profile_validation_allows_only_same_effective_profile() {
        let mut current = SessionSettingsValues::default();
        current.0.insert(
            HERMES_PROFILE_SETTING.to_string(),
            SessionSettingValue::String("work".to_string()),
        );
        let mut same = SessionSettingsValues::default();
        same.0.insert(
            HERMES_PROFILE_SETTING.to_string(),
            SessionSettingValue::String("work".to_string()),
        );
        assert!(validate_runtime_session_settings_update(&current, &same).is_ok());

        let mut changed = same;
        changed.0.insert(
            HERMES_PROFILE_SETTING.to_string(),
            SessionSettingValue::String("other".to_string()),
        );
        assert!(validate_runtime_session_settings_update(&current, &changed).is_err());
        let mut removed = SessionSettingsValues::default();
        removed.0.insert(
            HERMES_PROFILE_SETTING.to_string(),
            SessionSettingValue::Null,
        );
        assert!(validate_runtime_session_settings_update(&current, &removed).is_err());

        let default = SessionSettingsValues::default();
        let mut explicit_default = SessionSettingsValues::default();
        explicit_default.0.insert(
            HERMES_PROFILE_SETTING.to_string(),
            SessionSettingValue::String(hermes_config_default_profile().to_string()),
        );
        assert!(
            validate_runtime_session_settings_update(&default, &explicit_default).is_ok(),
            "absent and explicit default are the same effective profile"
        );
    }

    #[test]
    fn hermes_session_catalog_reflects_system_overlay_support() {
        let value = json!({
            "sessions": [{
                "id": "stored",
                "title": "Resumable Hermes session",
                "started_at": 1_700_000_000.0
            }]
        });
        let sessions = parse_session_list(&value, true).expect("local session catalog");

        assert_eq!(sessions.len(), 1);
        assert!(sessions[0].resumable);
        let sessions = parse_session_list(&value, false).expect("remote session catalog");
        assert!(!sessions[0].resumable);
    }

    #[test]
    fn hermes_remote_resume_catalog_matches_the_instruction_overlay_gate() {
        let remote_roots = vec!["ssh://builder.example/work/repo".to_string()];
        let local_roots = vec!["/work/repo".to_string()];
        let plain = ResolvedSpawnConfig::default();
        let protected = ResolvedSpawnConfig {
            access_mode: BackendAccessMode::ReadOnly,
            ..ResolvedSpawnConfig::default()
        };

        assert!(session_is_resumable_for_workspace_roots(
            &remote_roots,
            &plain
        ));
        assert!(!session_is_resumable_for_workspace_roots(
            &remote_roots,
            &protected
        ));
        assert!(session_is_resumable_for_workspace_roots(
            &local_roots,
            &protected
        ));
    }

    #[test]
    fn hermes_session_create_params_include_model_provider_reasoning_and_fast() {
        let mut settings = SessionSettingsValues::default();
        settings.0.insert(
            "model".to_string(),
            SessionSettingValue::String(encode_model_option_value(
                "minimax/minimax-m2.7",
                Some("openrouter"),
            )),
        );
        settings.0.insert(
            "reasoning_effort".to_string(),
            SessionSettingValue::String("none".to_string()),
        );
        settings
            .0
            .insert("fast".to_string(), SessionSettingValue::Bool(true));

        let params = build_session_create_params(&[], &settings, None).expect("params");

        assert_eq!(params["model"], "minimax/minimax-m2.7");
        assert_eq!(params["provider"], "openrouter");
        assert_eq!(params["reasoning_effort"], "none");
        assert_eq!(params["fast"], true);
    }

    #[test]
    fn hermes_model_option_value_round_trips_including_delimiter_like_ids() {
        // A model id containing the legacy delimiter must survive the round-trip.
        let model = "weird --provider embedded/model";
        let provider = "openrouter";
        let encoded = encode_model_option_value(model, Some(provider));
        let parsed = parse_hermes_model_setting(&encoded).expect("round-trips");
        assert_eq!(parsed.model, model);
        assert_eq!(parsed.provider.as_deref(), Some(provider));

        // No provider.
        let encoded = encode_model_option_value("bare/model", None);
        let parsed = parse_hermes_model_setting(&encoded).expect("round-trips");
        assert_eq!(parsed.model, "bare/model");
        assert_eq!(parsed.provider, None);

        // Legacy packed string still parses for previously persisted values.
        let legacy =
            parse_hermes_model_setting("legacy/model --provider anthropic").expect("legacy parses");
        assert_eq!(legacy.model, "legacy/model");
        assert_eq!(legacy.provider.as_deref(), Some("anthropic"));
    }

    /// Build a schema from one `model.options` payload as if it were the only
    /// (default) profile — the pre-profile schema shape these tests pin.
    fn schema_from_single_profile_payload(
        payload: &Value,
    ) -> Result<SessionSettingsSchema, String> {
        let profiles = vec![HermesProfileRef {
            name: protocol::hermes_config::HERMES_DEFAULT_PROFILE.to_string(),
            home_dir: PathBuf::from("/nonexistent-hermes-home"),
        }];
        session_schema_probe_from_model_options(
            &profiles,
            vec![Ok(payload.clone())],
            &HashMap::new(),
        )
        .map(|probe| probe.schema)
    }

    #[test]
    fn hermes_model_options_schema_uses_authenticated_provider_models() {
        let schema = schema_from_single_profile_payload(&json!({
            "provider": "openrouter",
            "model": "minimax/minimax-m2.7",
            "providers": [
                {
                    "slug": "openrouter",
                    "name": "OpenRouter",
                    "authenticated": true,
                    "models": ["minimax/minimax-m2.7", "anthropic/claude-sonnet-5"]
                },
                {
                    "slug": "anthropic",
                    "name": "Anthropic",
                    "authenticated": false,
                    "models": ["claude-opus"]
                }
            ]
        }))
        .expect("schema");

        assert_eq!(schema.backend_kind, BackendKind::Hermes);
        assert!(
            schema.fields.iter().all(|field| field.key != "provider"),
            "Hermes schema must not expose an independent provider dropdown"
        );

        let model_field = schema
            .fields
            .iter()
            .find(|field| field.key == "model")
            .expect("model field");
        match &model_field.field_type {
            SessionSettingFieldType::Select {
                options, default, ..
            } => {
                assert_eq!(options.len(), 2);
                assert_eq!(
                    options[0].value,
                    encode_model_option_value("minimax/minimax-m2.7", Some("openrouter"))
                );
                assert_eq!(
                    default.as_deref(),
                    Some(
                        encode_model_option_value("minimax/minimax-m2.7", Some("openrouter"))
                            .as_str()
                    )
                );
                assert!(
                    options[0].label.contains("OpenRouter"),
                    "flattened labels must include provider context"
                );
            }
            other => panic!("model must be Select, got {other:?}"),
        }
        assert!(
            schema
                .fields
                .iter()
                .any(|field| field.key == "reasoning_effort")
        );
        assert!(schema.fields.iter().any(|field| field.key == "fast"));
    }

    #[test]
    fn hermes_model_options_schema_does_not_infer_default_provider() {
        let schema = schema_from_single_profile_payload(&json!({
            "model": "shared/model",
            "providers": [
                {
                    "slug": "openrouter",
                    "name": "OpenRouter",
                    "authenticated": true,
                    "models": ["shared/model"]
                },
                {
                    "slug": "fallback",
                    "name": "Fallback",
                    "authenticated": true,
                    "models": ["shared/model"]
                }
            ]
        }))
        .expect("schema");

        let model_field = schema
            .fields
            .iter()
            .find(|field| field.key == "model")
            .expect("model field");
        match &model_field.field_type {
            SessionSettingFieldType::Select {
                options, default, ..
            } => {
                assert_eq!(options.len(), 2);
                assert!(
                    default.is_none(),
                    "missing top-level provider must not infer a provider-specific default"
                );
            }
            other => panic!("model must be Select, got {other:?}"),
        }
    }

    #[test]
    fn hermes_model_options_schema_rejects_malformed_top_level_selection() {
        for (name, payload, expected) in [
            (
                "non-string provider",
                json!({
                    "provider": 7,
                    "providers": [{
                        "slug": "openrouter",
                        "authenticated": true,
                        "models": ["anthropic/claude-haiku-4.5"]
                    }]
                }),
                "field provider must be a string",
            ),
            (
                "empty provider",
                json!({
                    "provider": " ",
                    "providers": [{
                        "slug": "openrouter",
                        "authenticated": true,
                        "models": ["anthropic/claude-haiku-4.5"]
                    }]
                }),
                "field provider must be non-empty",
            ),
            (
                "non-string model",
                json!({
                    "model": {},
                    "providers": [{
                        "slug": "openrouter",
                        "authenticated": true,
                        "models": ["anthropic/claude-haiku-4.5"]
                    }]
                }),
                "field model must be a string",
            ),
            (
                "empty model",
                json!({
                    "model": " ",
                    "providers": [{
                        "slug": "openrouter",
                        "authenticated": true,
                        "models": ["anthropic/claude-haiku-4.5"]
                    }]
                }),
                "field model must be non-empty",
            ),
        ] {
            let err = match schema_from_single_profile_payload(&payload) {
                Ok(_) => panic!("{name} should fail"),
                Err(err) => err,
            };
            assert!(
                err.contains(expected),
                "{name} error should contain {expected:?}, got {err:?}"
            );
        }
    }

    #[test]
    fn hermes_model_options_schema_rejects_malformed_provider_rows() {
        for (name, payload, expected) in [
            (
                "missing authenticated",
                json!({ "providers": [{ "slug": "openrouter", "models": [] }] }),
                "providers[0].authenticated must be a bool",
            ),
            (
                "non-bool authenticated",
                json!({ "providers": [{ "slug": "openrouter", "authenticated": "yes", "models": [] }] }),
                "providers[0].authenticated must be a bool",
            ),
            (
                "missing slug",
                json!({ "providers": [{ "authenticated": true, "models": [] }] }),
                "providers[0] missing required string field slug",
            ),
            (
                "empty slug",
                json!({ "providers": [{ "slug": " ", "authenticated": true, "models": [] }] }),
                "providers[0] field slug must be non-empty",
            ),
            (
                "non-array models",
                json!({ "providers": [{ "slug": "openrouter", "authenticated": true, "models": {} }] }),
                "providers[0] 'openrouter' missing models array",
            ),
            (
                "non-string model",
                json!({ "providers": [{ "slug": "openrouter", "authenticated": true, "models": [42] }] }),
                "providers[0] 'openrouter' models[0] must be a string",
            ),
            (
                "empty model",
                json!({ "providers": [{ "slug": "openrouter", "authenticated": true, "models": [" "] }] }),
                "providers[0] 'openrouter' models[0] must be non-empty",
            ),
        ] {
            let err = match schema_from_single_profile_payload(&payload) {
                Ok(_) => panic!("{name} should fail"),
                Err(err) => err,
            };
            assert!(
                err.contains(expected),
                "{name} error should contain {expected:?}, got {err:?}"
            );
        }
    }

    #[test]
    fn hermes_tool_state_is_scoped_to_one_turn() {
        let mut mapper = HermesEventMapper::default();

        assert!(matches!(
            mapper.map_event("message.start", None).as_slice(),
            [ChatEvent::StreamStart(_)]
        ));
        assert!(
            mapper
                .map_event(
                    "tool.start",
                    Some(json!({ "tool_id": "tool-1", "name": "shell" })),
                )
                .iter()
                .any(|event| matches!(event, ChatEvent::ToolRequest(_)))
        );
        assert!(
            mapper
                .map_event(
                    "tool.complete",
                    Some(json!({
                        "tool_id": "tool-1",
                        "name": "shell",
                        "result": { "ok": true }
                    })),
                )
                .iter()
                .any(
                    |event| matches!(event, ChatEvent::ToolExecutionCompleted(data) if data.success)
                )
        );
        let first_complete = mapper.map_event(
            "message.complete",
            Some(json!({ "text": "first", "status": "complete" })),
        );
        let first_end = first_complete
            .iter()
            .find_map(|event| match event {
                ChatEvent::StreamEnd(data) => Some(data),
                _ => None,
            })
            .expect("first turn StreamEnd");
        assert_eq!(first_end.message.tool_calls.len(), 1);
        assert_eq!(first_end.message.tool_calls[0].id, "tool-1");

        assert!(matches!(
            mapper.map_event("message.start", None).as_slice(),
            [ChatEvent::StreamStart(_)]
        ));
        let second_complete = mapper.map_event(
            "message.complete",
            Some(json!({ "text": "second", "status": "complete" })),
        );
        let second_end = second_complete
            .iter()
            .find_map(|event| match event {
                ChatEvent::StreamEnd(data) => Some(data),
                _ => None,
            })
            .expect("second turn StreamEnd");
        assert!(
            second_end.message.tool_calls.is_empty(),
            "second turn must not inherit first-turn tool calls"
        );
        assert!(
            second_complete.iter().all(|event| {
                !matches!(
                    event,
                    ChatEvent::MessageAdded(ChatMessage {
                        sender: MessageSender::Error,
                        ..
                    }) | ChatEvent::ToolExecutionCompleted(ToolExecutionCompletedData {
                        success: false,
                        ..
                    })
                )
            }),
            "second turn must not report stale unresolved/cancelled tool state: {second_complete:?}"
        );
    }

    #[test]
    fn hermes_nested_tool_errors_are_failed_completions() {
        let mut mapper = HermesEventMapper::default();
        let _ = mapper.map_event(
            "tool.start",
            Some(json!({
                "tool_id": "terminal-1",
                "name": "terminal",
                "args": { "command": "exit 7" }
            })),
        );
        let terminal = mapper.map_event(
            "tool.complete",
            Some(json!({
                "tool_id": "terminal-1",
                "name": "terminal",
                "result": {
                    "error": "command denied",
                    "exit_code": -1,
                    "status": "blocked"
                }
            })),
        );
        assert!(terminal.iter().any(|event| matches!(
            event,
            ChatEvent::ToolExecutionCompleted(ToolExecutionCompletedData {
                success: false,
                error: Some(error),
                ..
            }) if error == "command denied"
        )));

        let _ = mapper.map_event(
            "tool.start",
            Some(json!({
                "tool_id": "mcp-1",
                "name": "mcp_tyde_tyde_spawn_agent",
                "args": {
                    "name": "Hermes Child",
                    "prompt": "Inspect the failure path"
                }
            })),
        );
        let mcp = mapper.map_event(
            "tool.complete",
            Some(json!({
                "tool_id": "mcp-1",
                "name": "mcp_tyde_tyde_spawn_agent",
                "result": {
                    "isError": true,
                    "content": [{ "type": "text", "text": "missing prompt" }]
                }
            })),
        );
        assert!(
            mcp.iter().any(|event| matches!(
                event,
                ChatEvent::ToolExecutionCompleted(ToolExecutionCompletedData {
                    success: false,
                    error: Some(error),
                    ..
                }) if error == "missing prompt"
            )),
            "MCP tool failure must use its text content: {mcp:?}"
        );
    }

    #[test]
    fn hermes_late_completion_after_cancel_is_ignored() {
        let mut mapper = HermesEventMapper::default();
        let _ = mapper.map_event("message.start", None);
        let _ = mapper.map_event(
            "tool.start",
            Some(json!({
                "tool_id": "terminal-1",
                "name": "terminal",
                "args": { "command": "sleep 20" }
            })),
        );
        let cancelled = mapper.cancel_events("Operation cancelled");
        assert!(cancelled.iter().any(|event| matches!(
            event,
            ChatEvent::ToolExecutionCompleted(ToolExecutionCompletedData {
                success: false,
                tool_result: ToolExecutionResult::Cancelled { .. },
                ..
            })
        )));
        assert!(
            mapper
                .map_event(
                    "tool.complete",
                    Some(json!({
                        "tool_id": "terminal-1",
                        "name": "terminal",
                        "result": { "exit_code": -15 }
                    })),
                )
                .is_empty()
        );
        assert!(
            mapper
                .map_event(
                    "message.complete",
                    Some(json!({ "text": "already finished", "status": "complete" })),
                )
                .is_empty(),
            "the post-cancel settlement may race with a normal completion"
        );
        assert!(!mapper.awaiting_interrupted_complete);
    }

    #[test]
    fn hermes_background_terminal_emits_typed_lifecycle() {
        let mut mapper = HermesEventMapper::default();
        let request = mapper.map_event(
            "tool.start",
            Some(json!({
                "tool_id": "terminal-1",
                "name": "terminal",
                "args": {
                    "command": "sleep 8",
                    "background": true
                }
            })),
        );
        assert!(request.iter().any(|event| matches!(
            event,
            ChatEvent::ToolRequest(ToolRequest {
                tool_type: ToolRequestType::RunCommand { command, .. },
                ..
            }) if command == "sleep 8"
        )));

        let launched = mapper.map_event(
            "tool.complete",
            Some(json!({
                "tool_id": "terminal-1",
                "name": "terminal",
                "result": {
                    "session_id": "proc-1",
                    "exit_code": 0
                }
            })),
        );
        assert!(launched.iter().any(|event| matches!(
            event,
            ChatEvent::ToolProgress(ToolProgressData {
                tool_call_id,
                update: ToolProgressUpdate::BackgroundTask(BackgroundTaskState {
                    task_id,
                    status: BackgroundTaskStatus::Running,
                    ..
                }),
                ..
            }) if tool_call_id == "terminal-1" && task_id == "proc-1"
        )));

        let _ = mapper.map_event(
            "tool.start",
            Some(json!({
                "tool_id": "process-1",
                "name": "process",
                "args": {
                    "action": "wait",
                    "session_id": "proc-1"
                }
            })),
        );
        let completed = mapper.map_event(
            "tool.complete",
            Some(json!({
                "tool_id": "process-1",
                "name": "process",
                "result": { "exit_code": 0 }
            })),
        );
        assert!(completed.iter().any(|event| matches!(
            event,
            ChatEvent::ToolProgress(ToolProgressData {
                tool_call_id,
                update: ToolProgressUpdate::BackgroundTask(BackgroundTaskState {
                    task_id,
                    status: BackgroundTaskStatus::Completed,
                    ..
                }),
                ..
            }) if tool_call_id == "terminal-1" && task_id == "proc-1"
        )));
    }

    #[test]
    fn hermes_turn_interrupt_preserves_background_command() {
        let mut mapper = HermesEventMapper::default();
        let _ = mapper.map_event(
            "tool.start",
            Some(json!({
                "tool_id": "terminal-1",
                "name": "terminal",
                "args": {
                    "command": "sleep 8",
                    "background": true
                }
            })),
        );
        let _ = mapper.map_event(
            "tool.complete",
            Some(json!({
                "tool_id": "terminal-1",
                "name": "terminal",
                "result": {
                    "session_id": "proc-1",
                    "exit_code": 0
                }
            })),
        );

        let cancelled = mapper.cancel_events("Operation cancelled");

        assert!(
            mapper.background_tasks.contains_key("proc-1"),
            "ordinary turn interruption must preserve detached Hermes work"
        );
        assert!(cancelled.iter().all(|event| {
            !matches!(
                event,
                ChatEvent::ToolProgress(ToolProgressData {
                    update: ToolProgressUpdate::BackgroundTask(BackgroundTaskState {
                        status: BackgroundTaskStatus::Stopped,
                        ..
                    }),
                    ..
                })
            )
        }));
    }

    #[test]
    fn hermes_tool_generation_notice_is_not_a_warning() {
        let mut mapper = HermesEventMapper::default();
        assert!(
            mapper
                .map_event("tool.generating", Some(json!({ "name": "probe" })))
                .is_empty()
        );
    }

    #[test]
    fn hermes_gateway_preserves_authoritative_tool_arguments() {
        assert!(HERMES_MCP_GATEWAY_ENTRY.contains("payload[\"args\"] = args"));
        assert!(HERMES_MCP_GATEWAY_ENTRY.contains("_tyde_gateway_server._on_tool_start"));
    }

    #[test]
    fn hermes_gateway_preserves_native_cache_usage() {
        assert!(HERMES_MCP_GATEWAY_ENTRY.contains("session_cache_read_tokens"));
        assert!(HERMES_MCP_GATEWAY_ENTRY.contains("session_cache_write_tokens"));
        assert!(HERMES_MCP_GATEWAY_ENTRY.contains("session_reasoning_tokens"));
        assert!(HERMES_MCP_GATEWAY_ENTRY.contains("_tyde_gateway_server._get_usage"));
    }

    #[test]
    fn hermes_tyde_agent_tools_use_shared_typed_contracts() {
        let mut mapper = HermesEventMapper::default();
        let request = mapper.map_event(
            "tool.start",
            Some(json!({
                "tool_id": "spawn-1",
                "name": "mcp_tyde_tyde_spawn_agent",
                "args": {
                    "name": "Hermes Child",
                    "prompt": "Review this change"
                }
            })),
        );
        assert!(request.iter().any(|event| matches!(
            event,
            ChatEvent::ToolRequest(ToolRequest {
                tool_type: ToolRequestType::AgentSpawn {
                    prompt: Some(prompt),
                    name: Some(name),
                },
                ..
            }) if prompt == "Review this change" && name == "Hermes Child"
        )));

        let completion = mapper.map_event(
            "tool.complete",
            Some(json!({
                "tool_id": "spawn-1",
                "name": "mcp_tyde_tyde_spawn_agent",
                "args": {
                    "name": "Hermes Child",
                    "prompt": "Review this change"
                },
                "result": {
                    "result": "{\"agent_id\":\"agent-1\",\"name\":\"Hermes Child\",\"status\":\"thinking\"}"
                }
            })),
        );
        assert!(completion.iter().any(|event| matches!(
            event,
            ChatEvent::ToolProgress(ToolProgressData {
                update: ToolProgressUpdate::AgentControl(progress),
                ..
            }) if progress.agents.iter().any(|agent| agent.agent_id.0 == "agent-1")
        )));
    }

    #[test]
    fn hermes_native_delegation_is_a_typed_agent_spawn() {
        let mut mapper = HermesEventMapper::default();
        let events = mapper.map_event(
            "tool.start",
            Some(json!({
                "tool_id": "delegate-1",
                "name": "delegate_task",
                "args": { "goals": ["Inspect the protocol"] }
            })),
        );
        assert!(events.iter().any(|event| matches!(
            event,
            ChatEvent::ToolRequest(ToolRequest {
                tool_type: ToolRequestType::AgentSpawn {
                    prompt: Some(prompt),
                    ..
                },
                ..
            }) if prompt == "Inspect the protocol"
        )));
        assert_eq!(mapper.delegation_tools.len(), 1);
        assert_eq!(mapper.delegation_tools[0].tool_call_id, "delegate-1");
    }

    #[test]
    fn hermes_background_progress_carries_the_raw_request_name() {
        let mut mapper = HermesEventMapper::default();
        let _ = mapper.map_event(
            "tool.start",
            Some(json!({
                "tool_id": "terminal-1",
                "name": "Terminal",
                "args": {
                    "command": "sleep 8",
                    "background": true
                }
            })),
        );
        let launched = mapper.map_event(
            "tool.complete",
            Some(json!({
                "tool_id": "terminal-1",
                "name": "Terminal",
                "result": {
                    "session_id": "proc-1",
                    "exit_code": 0
                }
            })),
        );
        assert!(
            launched.iter().any(|event| matches!(
                event,
                ChatEvent::ToolProgress(ToolProgressData {
                    tool_call_id,
                    tool_name,
                    update: ToolProgressUpdate::BackgroundTask(BackgroundTaskState {
                        status: BackgroundTaskStatus::Running,
                        ..
                    }),
                }) if tool_call_id == "terminal-1" && tool_name == "Terminal"
            )),
            "background progress must carry the emitted request's raw name: {launched:?}"
        );

        let output = mapper.map_event(
            "agent.terminal.output",
            Some(json!({ "process_id": "proc-1", "text": "tick" })),
        );
        assert!(
            output.iter().any(|event| matches!(
                event,
                ChatEvent::ToolProgress(ToolProgressData {
                    tool_call_id,
                    tool_name,
                    ..
                }) if tool_call_id == "terminal-1" && tool_name == "Terminal"
            )),
            "terminal output frames must carry the emitted request's raw name: {output:?}"
        );
    }

    #[test]
    fn hermes_background_progress_never_attaches_to_a_reused_tool_id() {
        let mut mapper = HermesEventMapper::default();
        let _ = mapper.map_event("message.start", None);
        let _ = mapper.map_event(
            "tool.start",
            Some(json!({
                "tool_id": "tool-1",
                "name": "Terminal",
                "args": {
                    "command": "sleep 8",
                    "background": true
                }
            })),
        );
        let _ = mapper.map_event(
            "tool.complete",
            Some(json!({
                "tool_id": "tool-1",
                "name": "Terminal",
                "result": {
                    "session_id": "proc-1",
                    "exit_code": 0
                }
            })),
        );
        let _ = mapper.map_event(
            "message.complete",
            Some(json!({ "text": "launched", "status": "complete" })),
        );

        let cross_turn = mapper.map_event(
            "agent.terminal.output",
            Some(json!({ "process_id": "proc-1", "text": "tick" })),
        );
        assert!(
            cross_turn.iter().any(|event| matches!(
                event,
                ChatEvent::ToolProgress(ToolProgressData {
                    tool_call_id,
                    tool_name,
                    ..
                }) if tool_call_id == "tool-1" && tool_name == "Terminal"
            )),
            "the anchor survives a turn reset while the id is unclaimed: {cross_turn:?}"
        );

        let _ = mapper.map_event(
            "tool.start",
            Some(json!({
                "tool_id": "tool-1",
                "name": "read_file",
                "args": { "path": "/tmp/x" }
            })),
        );
        assert!(
            mapper
                .map_event(
                    "agent.terminal.output",
                    Some(json!({ "process_id": "proc-1", "text": "tick" })),
                )
                .is_empty(),
            "a reused tool_call_id must not receive the stale task's frames"
        );

        let _ = mapper.map_event(
            "tool.start",
            Some(json!({
                "tool_id": "process-1",
                "name": "process",
                "args": {
                    "action": "wait",
                    "session_id": "proc-1"
                }
            })),
        );
        let waited = mapper.map_event(
            "tool.complete",
            Some(json!({
                "tool_id": "process-1",
                "name": "process",
                "result": { "exit_code": 0 }
            })),
        );
        assert!(
            waited.iter().all(|event| !matches!(
                event,
                ChatEvent::ToolProgress(ToolProgressData {
                    update: ToolProgressUpdate::BackgroundTask(_),
                    ..
                })
            )),
            "wait completion must not attach the stale task to the reused id: {waited:?}"
        );
    }

    #[test]
    fn hermes_tool_progress_uses_the_registered_request_name_first() {
        let mut mapper = HermesEventMapper::default();
        let _ = mapper.map_event(
            "tool.start",
            Some(json!({
                "tool_id": "tool-1",
                "name": "Terminal",
                "args": { "command": "pwd" }
            })),
        );
        let progress = mapper.map_event(
            "tool.progress",
            Some(json!({
                "tool_id": "tool-1",
                "name": "terminal",
                "output": "tick"
            })),
        );
        assert!(
            progress.iter().any(|event| matches!(
                event,
                ChatEvent::ToolProgress(ToolProgressData {
                    tool_call_id,
                    tool_name,
                    ..
                }) if tool_call_id == "tool-1" && tool_name == "Terminal"
            )),
            "the registered request name wins over the payload name: {progress:?}"
        );

        let unknown = mapper.map_event(
            "tool.progress",
            Some(json!({
                "tool_id": "ghost-1",
                "name": "custom_probe",
                "output": "tick"
            })),
        );
        assert!(
            unknown.iter().any(|event| matches!(
                event,
                ChatEvent::ToolProgress(ToolProgressData {
                    tool_call_id,
                    tool_name,
                    ..
                }) if tool_call_id == "ghost-1" && tool_name == "custom_probe"
            )),
            "ids the registry never saw still use the payload name: {unknown:?}"
        );
    }

    #[test]
    fn hermes_delegation_anchors_are_never_guessed_across_candidates() {
        let mut mapper = HermesEventMapper::default();
        let _ = mapper.map_event(
            "tool.start",
            Some(json!({
                "tool_id": "delegate-1",
                "name": "mcp_tyde_tyde_delegate_task",
                "args": { "goals": ["Goal A"] }
            })),
        );
        let _ = mapper.map_event(
            "tool.start",
            Some(json!({
                "tool_id": "delegate-2",
                "name": "delegate_task",
                "args": { "goals": ["Goal B"] }
            })),
        );

        let by_goal = mapper
            .resolve_delegation_anchor(&json!({}), "Goal A")
            .expect("goal match");
        assert_eq!(by_goal.tool_call_id, "delegate-1");
        assert_eq!(by_goal.tool_name, "mcp_tyde_tyde_delegate_task");

        let by_id = mapper
            .resolve_delegation_anchor(&json!({ "parent_tool_call_id": "delegate-2" }), "")
            .expect("explicit id match");
        assert_eq!(by_id.tool_call_id, "delegate-2");
        assert_eq!(by_id.tool_name, "delegate_task");

        assert!(
            mapper
                .resolve_delegation_anchor(&json!({ "parent_tool_call_id": "delegate-9" }), "")
                .is_none(),
            "an unknown explicit parent id must not fall back to another card"
        );
        assert!(
            mapper.resolve_delegation_anchor(&json!({}), "").is_none(),
            "two outstanding delegations with no goal match are ambiguous"
        );

        let _ = mapper.map_event(
            "tool.complete",
            Some(json!({
                "tool_id": "delegate-1",
                "name": "mcp_tyde_tyde_delegate_task",
                "result": { "summary": "done" }
            })),
        );
        let only_outstanding = mapper
            .resolve_delegation_anchor(&json!({}), "")
            .expect("single outstanding candidate is unambiguous");
        assert_eq!(only_outstanding.tool_call_id, "delegate-2");

        mapper.clear_turn_tool_state();
        assert!(mapper.delegation_tools.is_empty());
        assert!(
            mapper
                .resolve_delegation_anchor(&json!({}), "Goal B")
                .is_none(),
            "a prior turn's delegation card must not adopt a new turn's children"
        );
    }

    #[test]
    fn hermes_idless_children_never_share_a_synthetic_identity() {
        let mut issued = HashMap::new();
        let mut live: HashSet<String> = HashSet::new();

        let first =
            resolve_synthetic_subagent_id("hermes-subagent-0", |id| live.contains(id), &mut issued);
        assert_eq!(first, "hermes-subagent-0");
        live.insert(first.clone());

        let same =
            resolve_synthetic_subagent_id("hermes-subagent-0", |id| live.contains(id), &mut issued);
        assert_eq!(same, first, "events for the live child keep its id");

        live.remove(&first);
        let second =
            resolve_synthetic_subagent_id("hermes-subagent-0", |id| live.contains(id), &mut issued);
        assert_ne!(
            second, first,
            "a new id-less child must not inherit a finished child's identity"
        );
        live.insert(second.clone());
        let same_second =
            resolve_synthetic_subagent_id("hermes-subagent-0", |id| live.contains(id), &mut issued);
        assert_eq!(same_second, second);
    }

    #[test]
    fn hermes_subagent_progress_carries_the_delegation_request_name() {
        let handle = SubAgentHandle {
            event_tx: mpsc::unbounded_channel().0,
            model_usage_tx: mpsc::unbounded_channel().0,
            total_usage_tx: mpsc::unbounded_channel().0,
            agent_id: protocol::AgentId("agent-1".to_string()),
            name_update_tx: None,
        };
        let anchor = HermesDelegationAnchor {
            tool_call_id: "delegate-1".to_string(),
            tool_name: "mcp_tyde_tyde_delegate_task".to_string(),
        };
        let progress = hermes_subagent_progress(&handle, "Hermes Agent 1", &anchor, 3, false);
        assert_eq!(progress.tool_call_id, "delegate-1");
        assert_eq!(progress.tool_name, "mcp_tyde_tyde_delegate_task");
    }

    #[test]
    fn hermes_native_title_does_not_override_tyde_naming() {
        let mut mapper = HermesEventMapper::default();
        assert!(
            mapper
                .map_event(
                    "session.title",
                    Some(json!({ "session_id": "stored", "title": "Hermes title" })),
                )
                .is_empty()
        );
    }

    #[test]
    fn unsupported_gateway_method_is_recognized() {
        assert!(is_unsupported_gateway_method(
            "Hermes JSON-RPC error -32601: unknown method: session.context_breakdown"
        ));
        assert!(is_unsupported_gateway_method(
            "unknown method: session.title"
        ));
        assert!(!is_unsupported_gateway_method(
            "Hermes session.context_breakdown timed out"
        ));
        assert!(!is_unsupported_gateway_method(
            "Hermes JSON-RPC error -32000: internal error"
        ));
    }

    #[test]
    fn managed_toolset_entry_matches_alias_and_respects_enabled() {
        assert!(
            managed_mcp_toolset_entry(&json!({
                "name": "tyde", "enabled": true, "tools": ["mcp_tyde_probe"]
            })),
            "the gateway displays the managed toolset under its server alias"
        );
        assert!(
            managed_mcp_toolset_entry(&json!({
                "name": "mcp-tyde", "tools": ["mcp_tyde_probe"]
            })),
            "canonical name with a missing enabled field counts as enabled"
        );
        assert!(!managed_mcp_toolset_entry(&json!({
            "name": "tyde", "enabled": false, "tools": ["mcp_tyde_probe"]
        })));
        assert!(!managed_mcp_toolset_entry(&json!({
            "name": "tyde", "enabled": true, "tools": []
        })));
        assert!(!managed_mcp_toolset_entry(&json!({
            "name": "file", "enabled": true, "tools": ["read_file"]
        })));
    }

    #[test]
    fn failure_message_absorbs_buffered_stderr_tail() {
        let empty = VecDeque::new();
        assert_eq!(
            format_failure_with_stderr_tail("Hermes gateway exited".to_string(), &empty),
            "Hermes gateway exited",
            "no buffered stderr should leave the message untouched"
        );

        let mut tail = VecDeque::new();
        tail.push_back("API call failed (attempt 3/3): AssertionError".to_string());
        tail.push_back("Error: model rejected the request".to_string());
        let message = format_failure_with_stderr_tail("Hermes gateway exited".to_string(), &tail);
        assert!(message.starts_with("Hermes gateway exited"));
        assert!(
            message.contains("AssertionError") && message.contains("model rejected the request"),
            "the failure must carry the buffered stderr so the cause is not silenced: {message}"
        );
    }

    #[test]
    fn hermes_reasoning_only_completion_suppresses_raw_reasoning_and_warns() {
        let mut mapper = HermesEventMapper::default();
        let _ = mapper.map_event("message.start", None);
        let _ = mapper.map_event("reasoning.delta", Some(json!({ "text": "thinking" })));

        let events = mapper.map_event(
            "message.complete",
            Some(json!({ "text": "", "status": "complete" })),
        );

        let end = events
            .iter()
            .find_map(|event| match event {
                ChatEvent::StreamEnd(data) => Some(data),
                _ => None,
            })
            .expect("StreamEnd");
        assert_eq!(end.message.content, "");
        assert!(end.message.reasoning.is_none());
        assert!(
            events.iter().any(|event| matches!(
                event,
                ChatEvent::MessageAdded(ChatMessage {
                    sender: MessageSender::Warning,
                    content,
                    ..
                }) if content.contains("reasoning only")
            )),
            "reasoning-only completions must be visible: {events:?}"
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, ChatEvent::TypingStatusChanged(false))),
            "reasoning-only completions must clear typing: {events:?}"
        );
        assert!(
            events.iter().all(|event| !matches!(
                event,
                ChatEvent::MessageAdded(ChatMessage {
                    sender: MessageSender::Error,
                    content,
                    ..
                }) if content.contains("missing required string field text")
            )),
            "empty final text must not be a missing-text protocol error: {events:?}"
        );
        assert!(
            events
                .iter()
                .all(|event| !format!("{event:?}").contains("thinking")),
            "raw Hermes reasoning leaked into events: {events:?}"
        );
    }

    #[test]
    fn hermes_empty_message_delta_is_noop() {
        let mut mapper = HermesEventMapper::default();
        let start = mapper.map_event("message.start", None);
        assert!(matches!(start.as_slice(), [ChatEvent::StreamStart(_)]));

        let events = mapper.map_event("message.delta", Some(json!({ "text": "" })));

        assert!(events.is_empty(), "empty deltas must be no-ops: {events:?}");
        assert!(
            mapper.current_message_id.is_some(),
            "empty deltas must not close the stream"
        );
    }

    #[test]
    fn hermes_completion_preserves_streamed_text_and_new_final_suffix() {
        let mut mapper = HermesEventMapper::default();
        let _ = mapper.map_event("message.start", None);
        let _ = mapper.map_event(
            "message.delta",
            Some(json!({ "text": "Checked the repository before the tool." })),
        );
        let _ = mapper.map_event(
            "message.delta",
            Some(json!({ "text": " The tool completed." })),
        );

        let events = mapper.map_event(
            "message.complete",
            Some(json!({
                "text": "Final recommendation not present in the deltas.",
                "status": "complete"
            })),
        );
        let content = events
            .iter()
            .find_map(|event| match event {
                ChatEvent::StreamEnd(data) => Some(data.message.content.as_str()),
                _ => None,
            })
            .expect("StreamEnd");

        assert!(
            content.starts_with("Checked the repository before the tool."),
            "{content}"
        );
        assert!(
            content.ends_with("Final recommendation not present in the deltas."),
            "{content}"
        );
    }

    #[test]
    fn hermes_tool_offsets_count_unicode_scalars() {
        let mut mapper = HermesEventMapper::default();
        let mut events = mapper.map_event("message.start", None);
        events.extend(mapper.map_event("message.delta", Some(json!({ "text": "Pré🙂 " }))));
        events.extend(mapper.map_event(
            "tool.start",
            Some(json!({
                "tool_id": "terminal-1",
                "name": "terminal",
                "args": { "command": "printf LIVE_TOOL_OK" }
            })),
        ));
        events.extend(mapper.map_event(
            "tool.complete",
            Some(json!({
                "tool_id": "terminal-1",
                "name": "terminal",
                "result": { "exit_code": 0, "stdout": "LIVE_TOOL_OK" }
            })),
        ));
        events.extend(mapper.map_event(
            "message.complete",
            Some(json!({ "text": "Pré🙂 ", "status": "complete" })),
        ));

        let end = events
            .iter()
            .find_map(|event| match event {
                ChatEvent::StreamEnd(data) => Some(data),
                _ => None,
            })
            .expect("StreamEnd");
        assert_eq!(end.message.content, "Pré🙂 ");
        assert_eq!(end.message.tool_calls.len(), 1);
        assert_eq!(end.message.tool_calls[0].id, "terminal-1");
        assert_eq!(
            end.message.tool_calls[0].content_offset,
            Some(5),
            "offsets count Unicode scalar values, not UTF-8 bytes"
        );
        assert_ne!("Pré🙂 ".len(), 5);
    }

    #[test]
    fn hermes_provider_requests_are_distinct_chat_messages() {
        let mut mapper = HermesEventMapper::default();
        let mut events = mapper.map_event("message.start", None);
        events.extend(mapper.map_event(
            "message.delta",
            Some(json!({ "text": "I will inspect the file." })),
        ));
        events.extend(mapper.map_event(
            "tool.start",
            Some(json!({
                "tool_id": "read-1",
                "name": "read_file",
                "args": { "path": "src/main.rs" }
            })),
        ));
        events.extend(mapper.map_event(
            "tool.complete",
            Some(json!({
                "tool_id": "read-1",
                "name": "read_file",
                "result": { "content": "fn main() {}" }
            })),
        ));

        // The tool cannot execute until the first provider response has ended,
        // so output arriving after its completion belongs to a new request.
        events.extend(mapper.map_event(
            "provider.request.start",
            Some(json!({
                "iteration": 2,
                "usage": { "input": 10, "output": 4, "total": 14 }
            })),
        ));
        events.extend(mapper.map_event(
            "message.delta",
            Some(json!({ "text": "The file contains an empty main function." })),
        ));
        events.extend(mapper.map_event(
            "message.complete",
            Some(json!({
                "text": "The file contains an empty main function.",
                "status": "complete"
            })),
        ));

        let messages = events
            .iter()
            .filter_map(|event| match event {
                ChatEvent::StreamEnd(data) => Some(&data.message),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            messages.len(),
            2,
            "each Hermes provider request must produce its own Tyde chat message"
        );
        assert_eq!(messages[0].content, "I will inspect the file.");
        assert_eq!(messages[0].tool_calls.len(), 1);
        assert_eq!(
            messages[1].content,
            "The file contains an empty main function."
        );
        assert!(messages[1].tool_calls.is_empty());
    }

    #[test]
    fn hermes_same_offset_tools_keep_observed_order() {
        let mut mapper = HermesEventMapper::default();
        let _ = mapper.map_event("message.start", None);
        let _ = mapper.map_event("message.delta", Some(json!({ "text": "PRE" })));
        for tool_id in ["tool-b", "tool-a"] {
            let _ = mapper.map_event(
                "tool.start",
                Some(json!({ "tool_id": tool_id, "name": "terminal" })),
            );
            let _ = mapper.map_event(
                "tool.complete",
                Some(json!({
                    "tool_id": tool_id,
                    "name": "terminal",
                    "result": { "exit_code": 0 }
                })),
            );
        }

        let events = mapper.map_event(
            "message.complete",
            Some(json!({ "text": "POST", "status": "complete" })),
        );
        let tools = events
            .iter()
            .find_map(|event| match event {
                ChatEvent::StreamEnd(data) => Some(&data.message.tool_calls),
                _ => None,
            })
            .expect("StreamEnd tools");
        assert_eq!(
            tools
                .iter()
                .map(|tool| tool.id.as_str())
                .collect::<Vec<_>>(),
            vec!["tool-b", "tool-a"]
        );
        assert_eq!(
            tools
                .iter()
                .map(|tool| tool.content_offset)
                .collect::<Vec<_>>(),
            vec![Some(3), Some(3)]
        );
    }

    #[test]
    fn hermes_reconciliation_reanchors_or_invalidates_tool_offsets() {
        let mut reanchored = HermesEventMapper::default();
        let _ = reanchored.map_event("message.start", None);
        let _ = reanchored.map_event("message.delta", Some(json!({ "text": "PRE" })));
        let _ = reanchored.map_event(
            "tool.start",
            Some(json!({ "tool_id": "tool-1", "name": "terminal" })),
        );
        let _ = reanchored.map_event(
            "tool.complete",
            Some(json!({
                "tool_id": "tool-1",
                "name": "terminal",
                "result": { "exit_code": 0 }
            })),
        );
        let events = reanchored.map_event(
            "message.complete",
            Some(json!({ "text": "  PRE extended", "status": "complete" })),
        );
        let message = events
            .iter()
            .find_map(|event| match event {
                ChatEvent::StreamEnd(data) => Some(&data.message),
                _ => None,
            })
            .expect("reanchored StreamEnd");
        assert_eq!(message.content, "  PRE extended");
        assert_eq!(message.tool_calls[0].content_offset, Some(5));

        let mut invalidated = HermesEventMapper::default();
        let _ = invalidated.map_event("message.start", None);
        let _ = invalidated.map_event("message.delta", Some(json!({ "text": "PRE   " })));
        let _ = invalidated.map_event(
            "tool.start",
            Some(json!({ "tool_id": "tool-2", "name": "terminal" })),
        );
        let _ = invalidated.map_event(
            "tool.complete",
            Some(json!({
                "tool_id": "tool-2",
                "name": "terminal",
                "result": { "exit_code": 0 }
            })),
        );
        let events = invalidated.map_event(
            "message.complete",
            Some(json!({ "text": "unequal final text", "status": "complete" })),
        );
        let message = events
            .iter()
            .find_map(|event| match event {
                ChatEvent::StreamEnd(data) => Some(&data.message),
                _ => None,
            })
            .expect("invalidated StreamEnd");
        assert_eq!(message.content, "PRE\n\nunequal final text");
        assert_eq!(
            message.tool_calls[0].content_offset, None,
            "removed streamed prefix text must invalidate the observed position"
        );
    }

    #[test]
    fn hermes_completion_does_not_duplicate_repeated_final_text() {
        let mut mapper = HermesEventMapper::default();
        let _ = mapper.map_event("message.start", None);
        let _ = mapper.map_event("message.delta", Some(json!({ "text": "hello" })));

        let events = mapper.map_event(
            "message.complete",
            Some(json!({ "text": "hello", "status": "complete" })),
        );
        let content = events
            .iter()
            .find_map(|event| match event {
                ChatEvent::StreamEnd(data) => Some(data.message.content.as_str()),
                _ => None,
            })
            .expect("StreamEnd");
        assert_eq!(content, "hello");
    }

    #[test]
    fn hermes_transient_status_uses_retry_channel_not_history() {
        let mut mapper = HermesEventMapper::default();
        let events = mapper.map_event(
            "status.update",
            Some(json!({
                "kind": "lifecycle",
                "text": "Retrying provider",
                "attempt": 2,
                "max_retries": 3,
                "backoff_ms": 500
            })),
        );

        assert!(matches!(
            events.as_slice(),
            [ChatEvent::RetryAttempt(RetryAttemptData {
                attempt: 2,
                max_retries: 3,
                backoff_ms: 500,
                ..
            })]
        ));
        assert!(
            events
                .iter()
                .all(|event| !matches!(event, ChatEvent::MessageAdded(_)))
        );
    }

    #[test]
    fn hermes_trailing_lifecycle_status_cannot_rearm_a_cancelled_turn() {
        let mut mapper = HermesEventMapper::default();
        let _ = mapper.map_event("message.start", None);
        let cancel_events = mapper.cancel_events("cancelled");
        assert!(
            cancel_events
                .iter()
                .any(|event| matches!(event, ChatEvent::TypingStatusChanged(false)))
        );

        let events = mapper.map_event(
            "status.update",
            Some(json!({
                "kind": "lifecycle",
                "text": "Retrying provider after an internal backoff"
            })),
        );

        assert!(events.is_empty());
        assert!(mapper.current_message_id.is_none());
    }

    #[test]
    fn hermes_compacting_status_stays_off_turn_lifecycle() {
        let mut mapper = HermesEventMapper::default();
        let _ = mapper.map_event("message.start", None);
        let events = mapper.map_event(
            "status.update",
            Some(json!({
                "kind": "compacting",
                "text": "Compacting context — summarizing earlier conversation"
            })),
        );

        assert!(events.is_empty());
        assert!(mapper.current_message_id.is_some());
    }

    #[test]
    fn hermes_approval_request_keeps_turn_busy() {
        let mut mapper = HermesEventMapper::default();
        let _ = mapper.map_event("message.start", None);

        let events = mapper.map_event(
            "approval.request",
            Some(json!({
                "description": "Run the command?",
                "command": "printf ok"
            })),
        );

        assert!(
            events
                .iter()
                .any(|event| matches!(event, ChatEvent::ToolRequest(_)))
        );
        assert!(
            events
                .iter()
                .all(|event| !matches!(event, ChatEvent::TypingStatusChanged(false)))
        );
    }

    #[test]
    fn hermes_error_completion_emits_one_error_without_assistant_masquerade() {
        let mut mapper = HermesEventMapper::default();
        let _ = mapper.map_event("message.start", None);
        let failure = "No allowed providers are available";

        let events = mapper.map_event(
            "message.complete",
            Some(json!({ "text": failure, "status": "error" })),
        );

        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(
                    event,
                    ChatEvent::MessageAdded(ChatMessage {
                        sender: MessageSender::Error,
                        content,
                        ..
                    }) if content == failure
                ))
                .count(),
            1
        );
        assert!(events.iter().all(|event| !matches!(
            event,
            ChatEvent::StreamEnd(StreamEndData { message })
                if message.content == failure
        )));
    }

    #[test]
    fn hermes_error_event_terminalizes_the_crashed_turn_once() {
        let mut mapper = HermesEventMapper::default();
        let _ = mapper.map_event("message.start", None);
        let _ = mapper.map_event(
            "tool.start",
            Some(json!({ "tool_id": "tool-1", "name": "terminal" })),
        );
        let failure = "No allowed providers are available";
        let events = mapper.map_event("error", Some(json!({ "message": failure })));

        assert!(mapper.current_message_id.is_none());
        assert!(
            events
                .iter()
                .any(|event| matches!(event, ChatEvent::StreamEnd(_)))
        );
        assert!(events.iter().any(|event| matches!(
            event,
            ChatEvent::ToolExecutionCompleted(ToolExecutionCompletedData {
                tool_result: ToolExecutionResult::Cancelled { .. },
                success: false,
                ..
            })
        )));
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(
                    event,
                    ChatEvent::MessageAdded(ChatMessage {
                        sender: MessageSender::Error,
                        content,
                        ..
                    }) if content == failure
                ))
                .count(),
            1
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, ChatEvent::TypingStatusChanged(false)))
        );
        assert!(
            events
                .iter()
                .all(|event| !matches!(event, ChatEvent::RetryAttempt(_)))
        );
    }

    #[test]
    fn hermes_message_complete_missing_status_defaults_to_complete() {
        let mut mapper = HermesEventMapper::default();
        let _ = mapper.map_event("message.start", None);

        let events = mapper.map_event("message.complete", Some(json!({ "text": "ok" })));

        assert!(events.iter().any(|event| matches!(
            event,
            ChatEvent::StreamEnd(StreamEndData { message }) if message.content == "ok"
        )));
        assert!(
            events
                .iter()
                .any(|event| matches!(event, ChatEvent::TypingStatusChanged(false)))
        );
    }

    #[test]
    fn hermes_message_complete_maps_turn_and_cumulative_usage() {
        let mut mapper = HermesEventMapper::default();
        let _ = mapper.map_event("message.start", None);

        let events = mapper.map_event(
            "message.complete",
            Some(json!({
                "text": "ok",
                "status": "complete",
                "usage": { "input": 3, "output": 4, "total": 7 },
                "cumulative_usage": { "input": 10, "output": 15, "total": 25 }
            })),
        );

        let end = events
            .iter()
            .find_map(|event| match event {
                ChatEvent::StreamEnd(data) => Some(data),
                _ => None,
            })
            .expect("StreamEnd");
        let usage = end.message.token_usage.as_ref().expect("token usage");
        assert_eq!(
            usage
                .turn
                .known_usage()
                .expect("known turn usage")
                .total_tokens,
            7
        );
        assert_eq!(
            usage
                .cumulative
                .known_usage()
                .expect("known cumulative usage")
                .total_tokens,
            25
        );
        assert!(matches!(
            usage.request,
            TokenUsageScope::Unavailable {
                reason: TokenUsageUnavailableReason::ProviderScopeAmbiguous
            }
        ));
    }

    #[test]
    fn hermes_message_complete_without_usage_emits_unavailable() {
        let mut mapper = HermesEventMapper::default();
        let _ = mapper.map_event("message.start", None);

        let events = mapper.map_event(
            "message.complete",
            Some(json!({ "text": "ok", "status": "complete" })),
        );

        let end = events
            .iter()
            .find_map(|event| match event {
                ChatEvent::StreamEnd(data) => Some(data),
                _ => None,
            })
            .expect("StreamEnd");
        let usage = end.message.token_usage.as_ref().expect("token usage");
        assert!(matches!(
            usage.turn,
            protocol::TokenUsageScope::Unavailable {
                reason: TokenUsageUnavailableReason::BackendDidNotReport
            }
        ));
    }

    #[test]
    fn hermes_session_cumulative_usage_is_differenced_per_turn() {
        let previous = token_usage_from_value(&json!({
            "input": 10,
            "output": 5,
            "total": 15,
            "cached_prompt_tokens": 2,
            "cache_creation_input_tokens": 3,
            "reasoning_tokens": 4
        }))
        .expect("previous usage");
        let current = token_usage_from_value(&json!({
            "input": 18,
            "output": 13,
            "total": 31,
            "cached_prompt_tokens": 7,
            "cache_creation_input_tokens": 8,
            "reasoning_tokens": 9
        }))
        .expect("current usage");

        let (turn, reset) = token_usage_delta(Some(&previous), &current);

        assert!(!reset);
        assert_eq!(turn.input_tokens, 8);
        assert_eq!(turn.output_tokens, 8);
        assert_eq!(turn.total_tokens, 16);
        assert_eq!(turn.cached_prompt_tokens, Some(5));
        assert_eq!(turn.cache_creation_input_tokens, Some(5));
        assert_eq!(turn.reasoning_tokens, Some(5));
    }

    #[test]
    fn hermes_usage_counter_decrease_starts_a_new_epoch() {
        let previous = token_usage_from_value(&json!({
            "input": 100,
            "output": 40,
            "total": 140,
            "reasoning_tokens": 20
        }))
        .expect("previous usage");
        let current = token_usage_from_value(&json!({
            "input": 9,
            "output": 3,
            "total": 12,
            "reasoning_tokens": 2
        }))
        .expect("current usage");

        let (turn, reset) = token_usage_delta(Some(&previous), &current);

        assert!(reset);
        assert_eq!(turn, current);
    }

    #[test]
    fn hermes_resumed_usage_epoch_keeps_first_turn_and_hides_partial_total() {
        let mut mapper = HermesEventMapper {
            cumulative_usage_incomplete: true,
            ..HermesEventMapper::default()
        };
        let first = TokenUsage {
            input_tokens: 12,
            output_tokens: 3,
            total_tokens: 15,
            ..TokenUsage::default()
        };

        let (first_turn, first_cumulative) = mapper.record_session_usage(first.clone());

        assert_eq!(first_turn, first);
        assert!(
            first_cumulative.is_none(),
            "a resumed runtime has no authoritative prior-session total"
        );

        let second = TokenUsage {
            input_tokens: 20,
            output_tokens: 7,
            total_tokens: 27,
            ..TokenUsage::default()
        };
        let (second_turn, second_cumulative) = mapper.record_session_usage(second);
        assert_eq!(second_turn.input_tokens, 8);
        assert_eq!(second_turn.output_tokens, 4);
        assert_eq!(second_turn.total_tokens, 12);
        assert!(second_cumulative.is_none());
    }

    #[test]
    fn hermes_usage_accepts_upstream_reasoning_alias() {
        let usage = token_usage_from_value(&json!({
            "input": 10,
            "output": 4,
            "total": 14,
            "reasoning": 3
        }))
        .expect("usage");

        assert_eq!(usage.reasoning_tokens, Some(3));
    }

    #[test]
    fn hermes_explicit_zero_usage_is_distinct_from_missing_usage() {
        assert!(token_usage_from_value(&json!({})).is_none());
        assert_eq!(
            token_usage_from_value(&json!({
                "input": 0,
                "output": 0,
                "total": 0
            }))
            .expect("explicit zero usage"),
            TokenUsage::default()
        );
    }

    #[test]
    fn hermes_context_breakdown_maps_native_categories() {
        let breakdown = context_breakdown_from_hermes(&json!({
            "context_used": 12_000,
            "context_max": 200_000,
            "estimated_total": 12_000,
            "categories": [
                { "id": "system_prompt", "tokens": 1_000 },
                { "id": "tool_definitions", "tokens": 2_000 },
                { "id": "mcp", "tokens": 300 },
                { "id": "subagent_definitions", "tokens": 200 },
                { "id": "conversation", "tokens": 7_000 },
                { "id": "rules", "tokens": 400 },
                { "id": "skills", "tokens": 500 },
                { "id": "memory", "tokens": 600 }
            ]
        }))
        .expect("native context breakdown");

        assert_eq!(breakdown.input_tokens, 12_000);
        assert_eq!(breakdown.context_window, 200_000);
        assert_eq!(breakdown.system_prompt_bytes, 4_000);
        assert_eq!(breakdown.tool_io_bytes, 10_000);
        assert_eq!(breakdown.conversation_history_bytes, 28_000);
        assert_eq!(breakdown.context_injection_bytes, 6_000);
    }

    #[test]
    fn hermes_context_breakdown_preserves_measured_total_with_estimated_categories() {
        let breakdown = context_breakdown_from_hermes(&json!({
            "context_used": 8_100,
            "context_max": 1_048_000,
            "estimated_total": 8_037,
            "categories": [
                { "id": "system_prompt", "tokens": 3_000 },
                { "id": "tool_definitions", "tokens": 2_000 },
                { "id": "conversation", "tokens": 3_037 }
            ]
        }))
        .expect("measured utilization with estimated categories");

        assert_eq!(breakdown.input_tokens, 8_100);
        assert_eq!(breakdown.system_prompt_bytes, 12_000);
        assert_eq!(breakdown.tool_io_bytes, 8_000);
        assert_eq!(breakdown.conversation_history_bytes, 12_148);
    }

    #[test]
    fn hermes_context_breakdown_keeps_large_unattributed_measured_remainder() {
        let breakdown = context_breakdown_from_hermes(&json!({
            "context_used": 61_000,
            "context_max": 1_048_000,
            "estimated_total": 8_100,
            "categories": [
                { "id": "tool_definitions", "tokens": 8_100 }
            ]
        }))
        .expect("measured utilization must survive incomplete attribution");

        assert_eq!(breakdown.input_tokens, 61_000);
        assert_eq!(breakdown.tool_io_bytes, 32_400);
        assert_eq!(breakdown.reasoning_bytes, 0);
    }

    #[test]
    fn hermes_message_complete_rejects_malformed_status() {
        for (payload, expected) in [
            (
                json!({ "text": "ok", "status": "" }),
                "status must be non-empty",
            ),
            (
                json!({ "text": "ok", "status": 7 }),
                "status must be a string",
            ),
        ] {
            let mut mapper = HermesEventMapper::default();
            let _ = mapper.map_event("message.start", None);

            let events = mapper.map_event("message.complete", Some(payload));

            assert!(
                events.iter().any(|event| matches!(
                    event,
                    ChatEvent::MessageAdded(ChatMessage {
                        sender: MessageSender::Error,
                        content,
                        ..
                    }) if content.contains(expected)
                )),
                "malformed status should surface {expected:?}: {events:?}"
            );
            assert!(
                events
                    .iter()
                    .any(|event| matches!(event, ChatEvent::TypingStatusChanged(false)))
            );
        }
    }

    #[test]
    fn hermes_empty_completion_without_reasoning_is_visible() {
        let mut mapper = HermesEventMapper::default();
        let _ = mapper.map_event("message.start", None);

        let events = mapper.map_event(
            "message.complete",
            Some(json!({ "text": "", "status": "complete" })),
        );

        let end = events
            .iter()
            .find_map(|event| match event {
                ChatEvent::StreamEnd(data) => Some(data),
                _ => None,
            })
            .expect("StreamEnd");
        assert_eq!(end.message.content, "");
        assert!(end.message.reasoning.is_none());
        assert!(
            events.iter().any(|event| matches!(
                event,
                ChatEvent::MessageAdded(ChatMessage {
                    sender: MessageSender::Error,
                    content,
                    ..
                }) if content.contains("without visible assistant text")
            )),
            "empty completions must be visible: {events:?}"
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, ChatEvent::TypingStatusChanged(false))),
            "empty completions must clear typing: {events:?}"
        );
    }

    #[test]
    fn hermes_mapper_error_closes_active_stream_tools_and_typing() {
        let mut mapper = HermesEventMapper::default();
        let _ = mapper.map_event("message.start", None);
        let _ = mapper.map_event(
            "tool.start",
            Some(json!({ "tool_id": "tool-1", "name": "shell" })),
        );

        let events = mapper.map_event("message.delta", Some(json!({})));

        assert!(
            events
                .iter()
                .any(|event| matches!(event, ChatEvent::StreamEnd(_))),
            "protocol errors must close open streams: {events:?}"
        );
        assert!(
            events.iter().any(|event| matches!(
                event,
                ChatEvent::ToolExecutionCompleted(ToolExecutionCompletedData {
                    tool_call_id,
                    success: false,
                    ..
                }) if tool_call_id == "tool-1"
            )),
            "protocol errors must complete open tools: {events:?}"
        );
        assert!(
            events.iter().any(|event| matches!(
                event,
                ChatEvent::MessageAdded(ChatMessage {
                    sender: MessageSender::Error,
                    content,
                    ..
                }) if content.contains("missing required string field text")
            )),
            "protocol errors must be visible: {events:?}"
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, ChatEvent::TypingStatusChanged(false))),
            "protocol errors must clear typing: {events:?}"
        );
        assert!(mapper.current_message_id.is_none());
        assert!(mapper.pending_tools.is_empty());
        assert!(mapper.turn_tools.is_empty());
    }

    #[tokio::test]
    async fn hermes_bad_prompt_status_clears_typing() {
        let _test_lock = TEST_HERMES_OVERRIDE_LOCK.lock().await;
        let dir = TempDir::new().expect("tempdir");
        let fake = write_fake_gateway(
            &dir,
            r#"
import json, sys
print(json.dumps({"jsonrpc":"2.0","method":"event","params":{"type":"gateway.ready","payload":{"skin":"default"}}}), flush=True)
for line in sys.stdin:
    req = json.loads(line)
    rid = req["id"]
    method = req["method"]
    if method == "session.create":
        print(json.dumps({"jsonrpc":"2.0","id":rid,"result":{"session_id":"live1","stored_session_id":"stored1","messages":[],"info":{}}}), flush=True)
    elif method == "prompt.submit":
        print(json.dumps({"jsonrpc":"2.0","id":rid,"result":{"status":"bogus"}}), flush=True)
    else:
        print(json.dumps({"jsonrpc":"2.0","id":rid,"result":{}}), flush=True)
"#,
        );
        let _guard = TestHermesPythonGuard::set(&fake);
        let (backend, mut events) = HermesBackend::spawn(
            vec![dir.path().to_string_lossy().to_string()],
            BackendSpawnConfig::default(),
            payload("hello"),
        )
        .await
        .expect("spawn fake hermes");

        let mut saw_error = false;
        let mut saw_typing_false = false;
        let mut observed = Vec::new();
        for _ in 0..8 {
            let event = timeout(Duration::from_secs(2), events.recv())
                .await
                .expect("event timeout")
                .expect("event stream open");
            observed.push(format!("{event:?}"));
            match event {
                ChatEvent::MessageAdded(ChatMessage {
                    sender: MessageSender::Error,
                    content,
                    ..
                }) if content.contains("unexpected status 'bogus'") => {
                    saw_error = true;
                }
                ChatEvent::TypingStatusChanged(false) if saw_error => {
                    saw_typing_false = true;
                    break;
                }
                _ => {}
            }
        }

        assert!(
            saw_error,
            "bad prompt status should emit a visible error; observed: {observed:#?}"
        );
        assert!(
            saw_typing_false,
            "bad prompt status should clear typing after the error; observed: {observed:#?}"
        );
        backend.shutdown().await;
    }

    #[test]
    fn todo_results_build_stable_typed_task_lists() {
        use protocol::TaskStatus;

        let mut ids = HashMap::new();
        let mut next_id = 0;
        let first = hermes_task_list_from_value(
            &json!({"todos": [
                {"id": "alpha", "content": "Alpha check", "status": "in_progress"},
                {"id": "beta", "content": "Beta check", "status": "pending"}
            ]}),
            &mut ids,
            &mut next_id,
        )
        .expect("first task list");
        let second = hermes_task_list_from_value(
            &json!({"todos": [
                {"id": "alpha", "content": "Alpha check", "status": "completed"},
                {"id": "beta", "content": "Beta check", "status": "in_progress"}
            ]}),
            &mut ids,
            &mut next_id,
        )
        .expect("second task list");

        assert_eq!(first.tasks[0].id, second.tasks[0].id);
        assert_eq!(first.tasks[1].id, second.tasks[1].id);
        assert!(matches!(second.tasks[0].status, TaskStatus::Completed));
        assert!(matches!(second.tasks[1].status, TaskStatus::InProgress));
    }

    #[test]
    fn history_replays_tool_only_messages_and_todo_state() {
        let events = hermes_history_to_chat_events(&json!({"messages": [
            {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call-1",
                    "function": {
                        "name": "todo",
                        "arguments": "{\"todos\":[{\"id\":\"alpha\",\"content\":\"Alpha check\",\"status\":\"in_progress\"}]}"
                    }
                }]
            },
            {
                "role": "tool",
                "content": "{\"todos\":[{\"id\":\"alpha\",\"content\":\"Alpha check\",\"status\":\"completed\"}]}",
                "tool_call_id": "call-1",
                "tool_name": "todo"
            },
            {
                "role": "tool",
                "name": "todo",
                "context": "Update task list"
            }
        ]}))
        .expect("history mapping");

        assert!(
            events
                .iter()
                .any(|event| matches!(event, ChatEvent::ToolRequest(_)))
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, ChatEvent::ToolExecutionCompleted(_)))
        );
        assert!(events.iter().any(|event| matches!(
            event,
            ChatEvent::TaskUpdate(tasks)
                if matches!(tasks.tasks[0].status, protocol::TaskStatus::Completed)
        )));
    }

    #[test]
    fn history_tool_completions_take_names_from_the_assistant_request() {
        let events = hermes_history_to_chat_events(&json!({"messages": [
            {
                "role": "assistant",
                "content": null,
                "tool_calls": [
                    {
                        "id": "call-1",
                        "function": {
                            "name": "todo",
                            "arguments": "{}"
                        }
                    },
                    {
                        "id": "call-2",
                        "function": {
                            "name": "Terminal",
                            "arguments": "{\"command\":\"pwd\"}"
                        }
                    }
                ]
            },
            {
                "role": "tool",
                "content": "{\"todos\":[{\"id\":\"alpha\",\"content\":\"Alpha check\",\"status\":\"completed\"}]}",
                "tool_call_id": "call-1"
            },
            {
                "role": "tool",
                "content": "/tmp",
                "tool_call_id": "call-2",
                "tool_name": "terminal"
            }
        ]}))
        .expect("history mapping");

        assert!(
            events.iter().any(|event| matches!(
                event,
                ChatEvent::ToolExecutionCompleted(ToolExecutionCompletedData {
                    tool_call_id,
                    tool_name,
                    ..
                }) if tool_call_id == "call-1" && tool_name == "todo"
            )),
            "a nameless tool record must resolve its request's name: {events:?}"
        );
        assert!(
            events.iter().any(|event| matches!(
                event,
                ChatEvent::TaskUpdate(tasks)
                    if matches!(tasks.tasks[0].status, protocol::TaskStatus::Completed)
            )),
            "todo reconstruction must survive a nameless tool record: {events:?}"
        );
        assert!(
            events.iter().any(|event| matches!(
                event,
                ChatEvent::ToolExecutionCompleted(ToolExecutionCompletedData {
                    tool_call_id,
                    tool_name,
                    ..
                }) if tool_call_id == "call-2" && tool_name == "Terminal"
            )),
            "the replayed request name is the authority over the record's own name: {events:?}"
        );
    }

    fn skill_resolved_config(selection: SkillSelection) -> ResolvedSpawnConfig {
        ResolvedSpawnConfig {
            skills: vec![
                crate::agent::customization::ResolvedSkill::test_fixture("axdb-ops", ""),
                crate::agent::customization::ResolvedSkill::test_fixture("eazy-ecs", ""),
            ],
            skill_selection: selection,
            ..ResolvedSpawnConfig::default()
        }
    }

    #[test]
    fn hermes_registers_the_resolved_skill_store_not_the_global_default() {
        let resolved = skill_resolved_config(SkillSelection::AllInstalled);

        assert_eq!(
            hermes_skill_roots(&resolved.skills).expect("resolved skill roots"),
            vec![PathBuf::from("/nonexistent/tyde-test-skills")]
        );
    }

    #[test]
    fn hermes_keeps_skills_available_when_mcp_selects_toolsets() {
        assert_eq!(
            hermes_selected_toolsets(vec!["terminal".to_string()], false, true),
            vec![
                "terminal".to_string(),
                "skills".to_string(),
                MANAGED_SERVER_NAME.to_string()
            ]
        );
        assert_eq!(
            hermes_selected_toolsets(vec!["terminal".to_string()], true, true),
            vec![MANAGED_SERVER_NAME.to_string()],
            "an empty allowlist remains authoritative"
        );
    }

    /// The failure this closes: a remote gateway reads another machine's config
    /// and another machine's disk, so naming Tyde's skills there promises
    /// instructions that do not exist. The model then reports every one of them
    /// as missing.
    #[test]
    fn remote_instructions_never_name_a_skill_hermes_cannot_load() {
        let resolved = skill_resolved_config(SkillSelection::AllInstalled);

        let local = render_hermes_spawn_instructions(&resolved, true).expect("local overlay");
        assert!(local.contains("axdb-ops"), "{local}");
        assert!(local.contains("skill discovery"), "{local}");

        assert_eq!(
            render_hermes_spawn_instructions(&resolved, false),
            None,
            "a remote session must not be handed skill names with nothing behind them"
        );
    }

    /// A registration Hermes did not take is the exact case that used to stop
    /// the spawn. It now costs the session its skills — and, critically, the
    /// instructions stop naming them, so the model is never told about a skill
    /// `skills_list` cannot see.
    #[test]
    fn a_failed_registration_drops_the_skills_from_the_prompt_not_the_session() {
        let resolved = skill_resolved_config(SkillSelection::AllInstalled);

        let (instructions, notice) = hermes_skill_exposure(&resolved, Some(Ok(())));
        assert!(
            instructions
                .as_deref()
                .is_some_and(|text| text.contains("axdb-ops")),
            "a registered store may be named: {instructions:?}"
        );
        assert_eq!(notice, None);

        let (instructions, notice) = hermes_skill_exposure(
            &resolved,
            Some(Err(
                "Hermes did not register the Tyde skills directory".to_string()
            )),
        );
        assert!(
            !instructions
                .as_deref()
                .unwrap_or_default()
                .contains("axdb-ops"),
            "a store Hermes did not register must not be named: {instructions:?}"
        );
        let notice = notice.expect("a failed registration must be reported");
        assert!(notice.contains("2 selected skill(s)"), "{notice}");
        assert!(notice.contains("did not register"), "{notice}");
        assert!(
            notice.contains("works normally otherwise"),
            "the session survives, and the notice says so: {notice}"
        );

        // Never attempted (remote, or nothing selected): no notice from here,
        // and nothing named.
        let (instructions, notice) = hermes_skill_exposure(&resolved, None);
        assert!(
            !instructions
                .as_deref()
                .unwrap_or_default()
                .contains("axdb-ops")
        );
        assert_eq!(notice, None);
    }

    /// A remote session keeps its workspace and loses its skills, whichever way
    /// they were selected. The selection type only changes how the loss is
    /// described — an explicitly named skill going missing is worth saying so.
    #[test]
    fn remote_hermes_drops_skills_with_a_notice_for_every_selection() {
        let explicit = skill_resolved_config(SkillSelection::Explicit);
        let explicit_notice = hermes_remote_skill_notice(&explicit, Some("builder.example"))
            .expect("an explicit selection is dropped with a notice, not refused");
        assert!(
            explicit_notice.contains("2 explicitly selected skill(s)"),
            "{explicit_notice}"
        );
        assert!(
            explicit_notice.contains("builder.example"),
            "{explicit_notice}"
        );

        let notice = hermes_remote_skill_notice(
            &skill_resolved_config(SkillSelection::AllInstalled),
            Some("builder.example"),
        )
        .expect("a Default agent starts remotely, but never silently");
        assert!(notice.contains("2 installed skill(s)"), "{notice}");
        assert!(notice.contains("builder.example"), "{notice}");

        // Local sessions are unaffected, and a skill-less remote session has
        // nothing to report either way.
        assert_eq!(hermes_remote_skill_notice(&explicit, None), None);
        assert_eq!(
            hermes_remote_skill_notice(&ResolvedSpawnConfig::default(), Some("builder.example")),
            None
        );
    }

    #[cfg(unix)]
    fn write_fake_hermes_python(path: &Path, script: &str) {
        use std::os::unix::fs::PermissionsExt;
        fs::write(path, script).expect("write fake python");
        let mut perms = fs::metadata(path).expect("launcher metadata").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms).expect("chmod launcher");
    }

    /// Registration is verified, not assumed. Hermes silently ignores an
    /// external skills directory it did not record, and a session that believed
    /// otherwise would name skills `skills_list` cannot see — the exact failure
    /// this work fixes.
    #[cfg(unix)]
    #[tokio::test]
    async fn hermes_skill_dir_registration_is_verified_not_assumed() {
        let dir = TempDir::new().expect("tempdir");
        let store = dir.path().join("skills");
        let target = |program: &Path| HermesSpawnTarget {
            program: program.to_string_lossy().to_string(),
            args: Vec::new(),
            env: HashMap::new(),
            cwd: None,
            remote_host: None,
            display_program: "hermes".to_string(),
            provider_version: None,
        };

        let confirming = dir.path().join("fake_python_confirms.sh");
        write_fake_hermes_python(
            &confirming,
            &format!(
                "#!/bin/sh\nprintf '[\"%s\"]\\n' \"{}\"\n",
                store.to_string_lossy()
            ),
        );
        register_hermes_skill_dir(&target(&confirming), &store)
            .await
            .expect("a registration Hermes confirms must be accepted");

        // Hermes reports a list without the store: registration silently did not
        // take, so the session must not start.
        let ignoring = dir.path().join("fake_python_ignores.sh");
        write_fake_hermes_python(&ignoring, "#!/bin/sh\necho '[\"/somewhere/else\"]'\n");
        let err = register_hermes_skill_dir(&target(&ignoring), &store)
            .await
            .expect_err("an unregistered store must fail the spawn");
        assert!(err.contains("did not register"), "{err}");
        assert!(err.contains("/somewhere/else"), "{err}");

        let failing = dir.path().join("fake_python_fails.sh");
        write_fake_hermes_python(&failing, "#!/bin/sh\necho 'boom' >&2\nexit 1\n");
        let err = register_hermes_skill_dir(&target(&failing), &store)
            .await
            .expect_err("a rejected registration must fail the spawn");
        assert!(err.contains("boom"), "{err}");
    }

    /// A `~`-spelled entry the user already configured is the same directory, so
    /// registration must not append a duplicate or claim it is missing.
    #[test]
    fn hermes_external_dir_comparison_expands_the_home_shorthand() {
        let home = crate::paths::home_dir().expect("home dir");
        assert_eq!(
            expand_hermes_path("~/.tyde/skills"),
            home.join(".tyde/skills")
        );
        assert_eq!(expand_hermes_path("~"), home);
        assert_eq!(
            expand_hermes_path("/absolute/skills"),
            PathBuf::from("/absolute/skills")
        );
        // Another user's home is not this one's.
        assert_eq!(
            expand_hermes_path("~someone/skills"),
            PathBuf::from("~someone/skills")
        );
    }

    #[test]
    fn compaction_capability_never_gates_on_provider_version() {
        for version in [
            None,
            Some("Hermes Agent v999.0.0"),
            Some("Hermes Agent v999.0.0-nightly.1"),
            Some("Hermes Agent development"),
            Some("malformed version output"),
        ] {
            let capability = hermes_compaction_capability(version);
            assert!(matches!(
                capability.availability,
                crate::backend::BackendCompactionAvailability::Native {
                    mechanism: BackendCompactionMechanism::JsonRpcRequest
                }
            ));
            assert_eq!(
                capability.provider_version.as_deref(),
                version.map(str::trim)
            );
            assert!(
                crate::backend::compaction::not_dispatched_for_capability(&capability).is_none()
            );
        }
    }

    #[test]
    fn transcript_guard_precedes_method_probe() {
        let capability = hermes_compaction_capability(None);
        assert!(matches!(
            hermes_compaction_pre_dispatch(&capability, false),
            Some(BackendCompactionStart::NotDispatched {
                reason: BackendCompactionNotDispatchedReason::NativeUnavailable(
                    BackendCompactionUnavailableReason::TranscriptNotAuthoritative
                ),
                fallback_safe: true,
            })
        ));
        assert!(
            hermes_compaction_pre_dispatch(
                &hermes_compaction_capability(Some("malformed version output")),
                true,
            )
            .is_none()
        );
    }

    #[test]
    fn typed_rpc_error_preserves_provider_code() {
        let mut pending = HashMap::new();
        let (reply_tx, reply_rx) = oneshot::channel();
        pending.insert(7, reply_tx);
        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        let mut ready = None;
        handle_gateway_stdout_line(
            r#"{"jsonrpc":"2.0","id":7,"error":{"code":4009,"message":"busy"}}"#,
            &mut pending,
            &event_tx,
            &mut ready,
        );
        let error = reply_rx
            .blocking_recv()
            .expect("gateway reply")
            .expect_err("provider error");
        assert_eq!(error.code, Some(4009));
        assert_eq!(error.message, "busy");
    }

    #[test]
    fn method_absence_is_cached_while_accepted_or_ambiguous_failures_stay_unsafe() {
        let stored = Arc::new(std::sync::Mutex::new(SessionId("stored".to_string())));
        let capability = Arc::new(std::sync::Mutex::new(hermes_compaction_capability(Some(
            "Hermes Agent v999.0.0-nightly.1",
        ))));
        let busy = classify_hermes_compaction_response(
            protocol::CompactionOperationId("busy".to_string()),
            "live".to_string(),
            SessionId("stored".to_string()),
            Err(HermesRpcError {
                code: Some(4009),
                message: "busy".to_string(),
            }),
            &stored,
            &capability,
        );
        assert_eq!(busy.dispatch, BackendCompactionDispatchState::Accepted);
        assert_eq!(busy.mutation, BackendCompactionMutationState::NotObserved);
        assert!(busy.outcome.is_err());

        let method_missing = classify_hermes_compaction_response(
            protocol::CompactionOperationId("method-missing".to_string()),
            "live".to_string(),
            SessionId("stored".to_string()),
            Err(HermesRpcError {
                code: Some(-32601),
                message: "Method not found".to_string(),
            }),
            &stored,
            &capability,
        );
        assert_eq!(
            method_missing.dispatch,
            BackendCompactionDispatchState::Rejected
        );
        assert_eq!(
            method_missing.mutation,
            BackendCompactionMutationState::NotObserved
        );
        let cached = capability
            .lock()
            .expect("Hermes compaction capability")
            .clone();
        assert!(matches!(
            &cached.availability,
            crate::backend::BackendCompactionAvailability::Unavailable {
                reason: BackendCompactionUnavailableReason::ManualTriggerAbsent
            }
        ));
        assert_eq!(
            cached.provider_version.as_deref(),
            Some("Hermes Agent v999.0.0-nightly.1")
        );
        assert!(matches!(
            crate::backend::compaction::not_dispatched_for_capability(&cached),
            Some(BackendCompactionStart::NotDispatched {
                fallback_safe: true,
                ..
            })
        ));

        let unknown = classify_hermes_compaction_response(
            protocol::CompactionOperationId("unknown".to_string()),
            "live".to_string(),
            SessionId("stored".to_string()),
            Err(HermesRpcError {
                code: Some(5005),
                message: "compress failed".to_string(),
            }),
            &stored,
            &capability,
        );
        assert_eq!(
            unknown.mutation,
            BackendCompactionMutationState::MayHaveMutated
        );

        let malformed = classify_hermes_compaction_response(
            protocol::CompactionOperationId("malformed".to_string()),
            "live".to_string(),
            SessionId("stored".to_string()),
            Ok(json!({"status":"compressed"})),
            &stored,
            &capability,
        );
        assert_eq!(
            malformed.mutation,
            BackendCompactionMutationState::MayHaveMutated
        );
        assert!(matches!(
            malformed.outcome,
            Err(BackendCompactionFailure {
                kind: BackendCompactionFailureKind::ProtocolViolation,
                ..
            })
        ));
    }
}
