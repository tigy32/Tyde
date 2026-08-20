use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use tokio::sync::{Mutex, mpsc, oneshot};

use protocol::{
    ChatMessageId, ContextBreakdown, ImageData, MessageTokenUsage, ModelInfo, ReasoningData,
    TokenUsage, TokenUsageUnavailableReason, ToolExecutionOutcome, ToolExecutionResult,
    ToolRequestType, ToolUseData,
};

use crate::acp::adapter::{
    AcpAgentAdapter, AcpAuthMethod, AcpAuthMethodHandling, AcpCapabilities, AcpRequestCtx,
    AcpSessionKind, adapter_for_spec,
};
use crate::acp::{
    AcpBridge, AcpInbound, acp_mcp_servers_json, extract_message_id, extract_text_from_update,
    extract_tool_call_id, map_plan_status, parse_tool_call_completion, parse_tool_call_request,
};
use crate::backend::turn_emitter::{
    AgentName, ResponseHandle, RetryAttemptPayload, StreamEndPayload, TurnEmitter,
};
use crate::backend::{
    BackendStartupError, SessionCommand, StartupMcpServer, backend_fork_unsupported_message,
    normalize_mcp_call_tool_result, render_combined_spawn_instructions,
};
use crate::process_env;
use crate::subprocess::ImageAttachment;

pub(crate) const KIRO_AGENT_NAME: &str = "kiro";
pub(crate) const KIRO_ADMIN_SESSION_SUBDIR: &str = ".tyde/kiro-admin";
pub(crate) const KIRO_EPHEMERAL_SESSION_SUBDIR: &str = ".tyde/kiro-ephemeral";
const KIRO_SCHEMA_PROBE_TIMEOUT: Duration = Duration::from_secs(30);
const KIRO_SCHEMA_PROBE_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);
const KIRO_PROMPT_MAX_RETRIES: u64 = 5;

fn kiro_prompt_error_is_retryable(error: &str) -> bool {
    let error = error.to_ascii_lowercase();
    [
        "dispatch failure",
        "response stream",
        "connection reset",
        "connection closed",
        "temporarily unavailable",
        "timed out",
        "timeout",
        "transport",
        "network",
    ]
    .iter()
    .any(|marker| error.contains(marker))
}

#[derive(Clone, Copy, Debug)]
enum KiroSchemaProbeStage {
    WorkspaceSetup,
    AcpSpawn,
    Initialize,
    SessionNew,
    ModelsList,
    Shutdown,
}

impl KiroSchemaProbeStage {
    fn label(self) -> &'static str {
        match self {
            Self::WorkspaceSetup => "workspace_setup",
            Self::AcpSpawn => "acp_spawn",
            Self::Initialize => "initialize",
            Self::SessionNew => "session_new",
            Self::ModelsList => "models_list",
            Self::Shutdown => "shutdown",
        }
    }
}

struct KiroSpawnMode<'a> {
    /// Which ACP agent to run, and how it deviates from the specification.
    adapter: Arc<dyn AcpAgentAdapter>,
    ephemeral: bool,
    admin_session: bool,
    initial_model: Option<&'a str>,
    ssh_host: Option<String>,
    startup_mcp_servers: &'a [StartupMcpServer],
    steering_content: Option<&'a str>,
    probe_deadline: Option<tokio::time::Instant>,
}

/// Build the adapter for the built-in Kiro agent.
///
/// `program_override` points the adapter at a specific binary; `None` lets it
/// resolve `kiro-cli-chat` as a sibling of `kiro-cli`.
fn kiro_adapter(program_override: Option<String>) -> Arc<dyn AcpAgentAdapter> {
    adapter_for_spec(&protocol::AcpAgentSpec {
        command: program_override.unwrap_or_default(),
        args: vec!["acp".to_string()],
        cwd: None,
        env: Default::default(),
        adapter: protocol::AcpAdapterId::Kiro,
    })
}

async fn await_kiro_stage<T>(
    deadline: Option<tokio::time::Instant>,
    stage: KiroSchemaProbeStage,
    future: impl Future<Output = Result<T, String>>,
) -> Result<T, String> {
    if let Some(deadline) = deadline {
        tracing::debug!(stage = stage.label(), "ACP schema probe stage started");
        let result = tokio::time::timeout_at(deadline, future)
            .await
            .map_err(|_| format!("ACP schema probe stage '{}' timed out", stage.label()))?
            .map_err(|err| format!("ACP schema probe stage '{}' failed: {err}", stage.label()))?;
        tracing::debug!(stage = stage.label(), "ACP schema probe stage completed");
        Ok(result)
    } else {
        future
            .await
            .map_err(|err| format!("Kiro {} failed: {err}", stage.label()))
    }
}

/// ACP protocol version Tyde speaks.
const ACP_CLIENT_PROTOCOL_VERSION: u32 = 1;

fn acp_initialize_params(adapter: &dyn AcpAgentAdapter) -> Value {
    json!({
        "protocolVersion": ACP_CLIENT_PROTOCOL_VERSION,
        "clientCapabilities": adapter.client_capabilities(),
        "clientInfo": {
            "name": "tyde",
            "title": "Tyde",
            "version": "0.1.0"
        }
    })
}

/// Read what the agent said it can do.
///
/// Anything absent is treated as unsupported. An agent that doesn't advertise
/// `loadSession` doesn't get `session/load` called on it, rather than Tyde
/// trying and interpreting the failure.
fn parse_capabilities(response: &Value) -> AcpCapabilities {
    let agent_caps = response.get("agentCapabilities");
    AcpCapabilities {
        protocol_version: response
            .get("protocolVersion")
            .and_then(Value::as_u64)
            .unwrap_or(ACP_CLIENT_PROTOCOL_VERSION as u64) as u32,
        load_session: agent_caps
            .and_then(|caps| caps.get("loadSession"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
        image: agent_caps
            .and_then(|caps| caps.get("promptCapabilities"))
            .and_then(|caps| caps.get("image"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
        // `sessionCapabilities.list` is an object, not a bool: the spec gates
        // the method on the key being present at all, and the object carries
        // only `_meta`. Treat any non-null value as support.
        session_list: agent_caps
            .and_then(|caps| caps.get("sessionCapabilities"))
            .and_then(|caps| caps.get("list"))
            .is_some_and(|value| !value.is_null()),
        auth_methods: response
            .get("authMethods")
            .and_then(Value::as_array)
            .map(|methods| {
                methods
                    .iter()
                    .filter_map(|method| {
                        let id = method
                            .get("id")
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|value| !value.is_empty())
                            .or_else(|| {
                                method
                                    .get("methodId")
                                    .and_then(Value::as_str)
                                    .map(str::trim)
                                    .filter(|value| !value.is_empty())
                            })?;
                        let optional_string = |key| {
                            method
                                .get(key)
                                .and_then(Value::as_str)
                                .map(str::trim)
                                .filter(|value| !value.is_empty())
                                .map(str::to_string)
                        };
                        Some(AcpAuthMethod {
                            id: id.to_string(),
                            name: optional_string("name"),
                            description: optional_string("description"),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default(),
        agent_info: response
            .get("agentInfo")
            .and_then(|info| info.get("name"))
            .and_then(Value::as_str)
            .map(str::to_string),
    }
}

/// Run `authenticate` when the agent advertises auth methods.
///
/// Tries each protocol method in advertised order and succeeds on the first
/// that works. Adapter-classified external setup is never sent to
/// `authenticate`; it becomes an actionable hard error if no later protocol
/// method succeeds. Tyde never proceeds to `session/new` unauthenticated.
async fn authenticate_if_required(
    bridge: &AcpBridge,
    capabilities: &AcpCapabilities,
    adapter: &dyn AcpAgentAdapter,
) -> Result<(), String> {
    if capabilities.auth_methods.is_empty() {
        return Ok(());
    }

    let agent = adapter.display_name();
    let mut last_error = None;
    let mut external_requirements = Vec::new();
    for method in &capabilities.auth_methods {
        match adapter.auth_method_handling(method) {
            AcpAuthMethodHandling::ProtocolAuthenticate => {
                match bridge
                    .request("authenticate", json!({ "methodId": method.id.as_str() }))
                    .await
                {
                    Ok(_) => {
                        tracing::debug!("{agent}: authenticated via ACP method '{}'", method.id);
                        return Ok(());
                    }
                    Err(err) => {
                        tracing::debug!("{agent}: ACP auth method '{}' failed: {err}", method.id);
                        last_error = Some(err);
                    }
                }
            }
            AcpAuthMethodHandling::ExternalSetup { instruction } => {
                external_requirements.push((&method.id, instruction));
            }
        }
    }

    if let [(method_id, instruction)] = external_requirements.as_slice() {
        return Err(format!(
            "{agent} authentication required via '{method_id}'. {instruction}"
        ));
    }
    if !external_requirements.is_empty() {
        let methods = external_requirements
            .iter()
            .map(|(method_id, instruction)| format!("'{method_id}': {instruction}"))
            .collect::<Vec<_>>()
            .join("; ");
        return Err(format!(
            "{agent} authentication requires external setup. Complete one of: {methods}"
        ));
    }

    Err(format!(
        "{agent} requires authentication and every advertised method failed \
         (tried: {}). Last error: {}",
        capabilities
            .auth_methods
            .iter()
            .map(|method| method.id.as_str())
            .collect::<Vec<_>>()
            .join(", "),
        last_error.unwrap_or_else(|| "no methods advertised".to_string())
    ))
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
        Self::spawn_for_agent(
            workspace_roots,
            None,
            initial_model,
            ssh_host,
            startup_mcp_servers,
            steering_content,
        )
        .await
    }

    /// Start a session for a specific ACP agent.
    ///
    /// `agent` of `None` means the built-in Kiro agent, which is what a
    /// session recorded before ACP launch profiles existed resumes as.
    pub async fn spawn_for_agent(
        workspace_roots: &[String],
        agent: Option<&protocol::AcpAgentSpec>,
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
                adapter: agent.map_or_else(|| kiro_adapter(None), adapter_for_spec),
                probe_deadline: None,
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
                adapter: kiro_adapter(None),
                probe_deadline: None,
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
                adapter: kiro_adapter(program_override),
                probe_deadline: None,
            },
        )
        .await
    }

    async fn spawn_schema_probe(
        workspace_roots: &[String],
        adapter: Arc<dyn AcpAgentAdapter>,
        probe_deadline: tokio::time::Instant,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Value>), String> {
        Self::spawn_with_mode(
            workspace_roots,
            KiroSpawnMode {
                ephemeral: true,
                admin_session: true,
                initial_model: None,
                ssh_host: None,
                startup_mcp_servers: &[],
                steering_content: None,
                adapter,
                probe_deadline: Some(probe_deadline),
            },
        )
        .await
    }

    async fn spawn_with_mode(
        workspace_roots: &[String],
        mode: KiroSpawnMode<'_>,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Value>), String> {
        let adapter = mode.adapter.clone();
        let roots = await_kiro_stage(
            mode.probe_deadline,
            KiroSchemaProbeStage::WorkspaceSetup,
            adapter.resolve_roots(
                workspace_roots,
                mode.ssh_host.as_deref(),
                AcpSessionKind {
                    admin_session: mode.admin_session,
                    ephemeral: mode.ephemeral,
                },
            ),
        )
        .await?;

        let mut spawn_spec = adapter.spawn_spec(&roots, mode.ssh_host.as_deref())?;
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

        let acp_program = spawn_spec.local_program.clone();
        let agent_label = adapter.display_name().to_string();
        let (bridge, inbound_rx) =
            await_kiro_stage(mode.probe_deadline, KiroSchemaProbeStage::AcpSpawn, async {
                AcpBridge::spawn(spawn_spec, mode.ssh_host.as_deref())
                    .await
                    .map_err(|err| {
                        format!("Failed to start {agent_label} executable '{acp_program}': {err}")
                    })
            })
            .await?;

        let initialize_response = await_kiro_stage(
            mode.probe_deadline,
            KiroSchemaProbeStage::Initialize,
            bridge.request("initialize", acp_initialize_params(adapter.as_ref())),
        )
        .await?;
        let capabilities = parse_capabilities(&initialize_response);

        // Adapter-specific external setup remains a hard gate; protocol auth
        // methods must succeed before a session can be created.
        authenticate_if_required(&bridge, &capabilities, adapter.as_ref()).await?;

        let session_result: Result<(String, Value), String> = async {
            let mut session_params = json!({
                "cwd": roots.session_cwd,
                "mcpServers": acp_mcp_servers_json(mode.startup_mcp_servers)
            });
            adapter.decorate_session_new(
                &mut session_params,
                &AcpRequestCtx {
                    session_id: "",
                    model: None,
                    mode: None,
                    system_prompt: mode.steering_content,
                    capabilities: &capabilities,
                },
            );
            let session_started = await_kiro_stage(
                mode.probe_deadline,
                KiroSchemaProbeStage::SessionNew,
                bridge.request("session/new", session_params),
            )
            .await?;

            let session_id = session_started
                .get("sessionId")
                .and_then(Value::as_str)
                .or_else(|| {
                    session_started
                        .get("session")
                        .and_then(|v| v.get("sessionId"))
                        .and_then(Value::as_str)
                })
                .ok_or_else(|| format!("{agent_label} session/new response missing sessionId"))?
                .to_string();

            Ok((session_id, session_started))
        }
        .await;

        let (session_id, session_started) = session_result?;

        let initial_model = extract_current_model(&session_started);
        let initial_mode = extract_current_mode(&session_started);

        let (event_tx, event_rx) = mpsc::unbounded_channel();

        let inner = Arc::new(KiroInner {
            adapter,
            capabilities,
            bridge,
            emitter: Arc::new(TurnEmitter::new_for_agent(
                event_tx,
                AgentName(KIRO_AGENT_NAME),
            )),
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
                active_response: None,
                active_stream_text: String::new(),
                active_stream_tool_calls: Vec::new(),
                active_tool_contexts: HashMap::new(),
                tool_call_aliases: HashMap::new(),
                completed_tool_call_ids: HashSet::new(),
                tool_call_occurrences: HashMap::new(),
                cancelled: false,
                provider_turn_quarantined: false,
                replaying_history: false,
                replay_session_id: None,
                replay_assistant_identity: None,
                replay_assistant_text: String::new(),
                replay_assistant_reasoning: String::new(),
                replay_assistant_message_emitted_since_user: false,
                replay_error: None,
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
            inner.emitter.session_started(&state.session_id);
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
    active_response: Option<ResponseHandle>,
    active_stream_text: String,
    active_stream_tool_calls: Vec<ToolUseData>,
    active_tool_contexts: HashMap<String, KiroToolContext>,
    tool_call_aliases: HashMap<String, String>,
    /// Canonical ids whose call already reached a terminal completion.
    /// Providers reuse short ids (`T1`, `call_1`) across sequential calls;
    /// merging a reused id into the finished call's identity would silently
    /// swallow the new call's request and completion downstream, so a new
    /// `tool_call` for a completed id mints a fresh occurrence id instead.
    completed_tool_call_ids: HashSet<String>,
    /// Occurrences seen per reused canonical id; monotonic for the life of
    /// the session so minted ids never collide with earlier occurrences.
    tool_call_occurrences: HashMap<String, u64>,
    cancelled: bool,
    provider_turn_quarantined: bool,
    replaying_history: bool,
    replay_session_id: Option<String>,
    replay_assistant_identity: Option<KiroReplayMessageIdentity>,
    replay_assistant_text: String,
    replay_assistant_reasoning: String,
    replay_assistant_message_emitted_since_user: bool,
    replay_error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct KiroReplayMessageIdentity {
    message_id: Option<ChatMessageId>,
}

impl KiroReplayMessageIdentity {
    fn new(message_id: Option<ChatMessageId>) -> Self {
        Self { message_id }
    }
}

#[derive(Clone)]
struct PendingToolCompletion {
    tool_name: String,
    tool_result: Value,
    success: bool,
    error: Option<String>,
}

#[derive(Clone)]
pub(crate) struct KiroToolContext {
    tool_name: String,
    pub(crate) tool_type: Value,
    is_mcp_tool: bool,
    request_emitted: bool,
    pending_completion: Option<PendingToolCompletion>,
}

fn kiro_is_startup_mcp_tool(tool_name: &str, servers: &[StartupMcpServer]) -> bool {
    servers.iter().any(|server| {
        let marker = format!("@{}/", server.name);
        tool_name
            .split_whitespace()
            .any(|part| part.contains(&marker))
            || tool_name.contains(&marker)
    })
}

struct KiroInner {
    adapter: Arc<dyn AcpAgentAdapter>,
    capabilities: AcpCapabilities,
    bridge: AcpBridge,
    emitter: Arc<TurnEmitter>,
    state: Mutex<KiroState>,
    shutting_down: AtomicBool,
    ssh_host: Option<String>,
}

impl KiroInner {
    async fn request_prompt_with_retry(&self, params: Value) -> Result<Value, String> {
        let mut attempt = 0u64;
        loop {
            match self.bridge.request("session/prompt", params.clone()).await {
                Ok(response) => return Ok(response),
                Err(error)
                    if attempt < KIRO_PROMPT_MAX_RETRIES
                        && kiro_prompt_error_is_retryable(&error) =>
                {
                    if self.state.lock().await.cancelled {
                        return Err(error);
                    }
                    attempt += 1;
                    let backoff_ms = 250u64
                        .saturating_mul(1u64 << attempt.saturating_sub(1))
                        .min(2_000);
                    eprintln!(
                        "TYDE KIRO PROMPT RETRY attempt={attempt} max_retries={KIRO_PROMPT_MAX_RETRIES} backoff_ms={backoff_ms} error={error}"
                    );
                    self.emitter.retry_attempt(RetryAttemptPayload {
                        attempt,
                        max_retries: KIRO_PROMPT_MAX_RETRIES,
                        error: &error,
                        backoff_ms,
                    });
                    tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                    if self.state.lock().await.cancelled {
                        return Err(error);
                    }
                }
                Err(error) => return Err(error),
            }
        }
    }

    async fn execute(&self, command: SessionCommand) -> Result<(), String> {
        match command {
            SessionCommand::SendMessage { message, images } => {
                self.state.lock().await.provider_turn_quarantined = false;
                self.emit_user_message_added(&message, images.as_deref());
                self.emitter.typing_status_changed(true);

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

                self.adapter.decorate_prompt(
                    &mut params,
                    &AcpRequestCtx {
                        session_id: &session_id,
                        model: model.as_deref(),
                        mode: mode.as_deref(),
                        system_prompt: steering.as_deref(),
                        capabilities: &self.capabilities,
                    },
                );

                self.state.lock().await.cancelled = false;

                let response = match self.request_prompt_with_retry(params).await {
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
                        if !self.shutting_down.load(Ordering::Acquire) {
                            self.abort_active_turn(&format!("Kiro request failed: {err}"))
                                .await;
                        }
                        return Err(err);
                    }
                };

                if let Err(err) = self.bridge.sync_inbound().await {
                    if !self.shutting_down.load(Ordering::Acquire) {
                        self.abort_active_turn(&format!("Kiro response stream failed: {err}"))
                            .await;
                    }
                    return Err(err);
                }

                if self.state.lock().await.provider_turn_quarantined {
                    return Ok(());
                }

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
                    // If the user initiated the cancel, `CancelConversation` already
                    // fired OperationCancelled + TypingStatusChanged — don't double-emit.
                    let user_initiated = {
                        let mut state = self.state.lock().await;
                        let was = state.cancelled;
                        state.cancelled = false;
                        was
                    };
                    if !user_initiated {
                        self.abort_active_turn("Operation cancelled").await;
                    }
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
                    self.abort_active_turn(&message).await;
                    self.emitter.backend_error(&message);
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
                let result = self
                    .bridge
                    .notify("session/cancel", json!({ "sessionId": session_id }))
                    .await;
                self.abort_active_turn("Operation cancelled").await;
                result
            }
            SessionCommand::GetSettings => {
                let state = self.state.lock().await;
                self.emitter.settings(json!({
                    "model": state.model,
                    "mode": state.mode,
                }));
                Ok(())
            }
            SessionCommand::ListSessions => self.list_sessions().await,
            SessionCommand::ResumeSession { session_id } => self.resume_session(session_id).await,
            SessionCommand::DeleteSession { session_id } => self.delete_session(session_id).await,
            SessionCommand::ListProfiles => {
                self.emitter.profiles_list(Vec::new());
                Ok(())
            }
            SessionCommand::SwitchProfile { profile_name: _ } => Ok(()),
            SessionCommand::GetModuleSchemas => {
                self.emitter.module_schemas(Vec::new());
                Ok(())
            }
            SessionCommand::ListModels => {
                let models = self.state.lock().await.known_models.clone();
                self.emitter.models_list(models);
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
                self.emitter.settings(json!({
                    "model": state.model,
                    "mode": state.mode,
                }));
                Ok(())
            }
        }
    }

    /// Enumerates sessions with the spec's `session/list`, following
    /// `nextCursor` until the agent stops paginating.
    ///
    /// The page count is bounded: a buggy agent that always echoes a cursor
    /// would otherwise spin here forever holding the session list open. Hitting
    /// the bound returns what was collected rather than failing, because a
    /// truncated list is more useful than none.
    async fn list_sessions_via_acp(&self) -> Result<Vec<BackendSession>, String> {
        const MAX_PAGES: usize = 100;

        let workspace_root = self.state.lock().await.workspace_root.clone();
        let mut sessions = Vec::new();
        let mut cursor: Option<String> = None;

        for page in 0..MAX_PAGES {
            let mut params = json!({});
            if !workspace_root.is_empty() {
                params["cwd"] = Value::String(workspace_root.clone());
            }
            if let Some(cursor) = &cursor {
                params["cursor"] = Value::String(cursor.clone());
            }

            let response = self.bridge.request("session/list", params).await?;
            let page_sessions = response
                .get("sessions")
                .and_then(Value::as_array)
                .ok_or_else(|| format!("ACP session/list response missing sessions: {response}"))?;
            for session in page_sessions {
                if let Some(session) = acp_session_info_to_backend_session(session) {
                    sessions.push(session);
                }
            }

            cursor = response
                .get("nextCursor")
                .and_then(Value::as_str)
                .map(str::to_string)
                .filter(|cursor| !cursor.is_empty());
            if cursor.is_none() {
                return Ok(sessions);
            }
            if page + 1 == MAX_PAGES {
                tracing::warn!(
                    pages = MAX_PAGES,
                    "ACP session/list kept paginating; returning a truncated list"
                );
            }
        }

        Ok(sessions)
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

        // `session/list` is the spec's own way to enumerate sessions, so an
        // agent that advertises it needs no adapter support at all. Adapters
        // only cover agents that list out of band: Kiro reads its own JSON
        // files (locally or over ssh) and advertises nothing.
        let listed = if self.capabilities.session_list {
            self.list_sessions_via_acp().await?
        } else {
            self.adapter.list_sessions(self.ssh_host.as_deref()).await?
        };

        let mut sessions = listed
            .into_iter()
            .filter(|session| excluded_session_id.as_deref() != Some(session.id.0.as_str()))
            .map(|session| {
                let last_modified = session.updated_at_ms.or(session.created_at_ms).unwrap_or(0);
                json!({
                    "id": session.id.0,
                    "session_id": session.id.0,
                    "title": session.title.unwrap_or_default(),
                    "created_at": session.created_at_ms.unwrap_or(last_modified),
                    "last_modified": last_modified,
                    "last_message_preview": "",
                    "workspace_root": session
                        .workspace_roots
                        .first()
                        .cloned()
                        .unwrap_or_default(),
                    "message_count": Value::Null,
                    "backend_kind": protocol::ACP_BACKEND,
                })
            })
            .collect::<Vec<_>>();

        sessions.sort_by(|a, b| {
            let a_ts = a.get("last_modified").and_then(Value::as_u64).unwrap_or(0);
            let b_ts = b.get("last_modified").and_then(Value::as_u64).unwrap_or(0);
            b_ts.cmp(&a_ts)
        });

        self.emitter.sessions_list(sessions);
        Ok(())
    }

    async fn delete_session(&self, session_id: String) -> Result<(), String> {
        let normalized = normalize_optional_string(&Value::String(session_id))
            .ok_or("Invalid session id".to_string())?;

        self.adapter
            .delete_session(&normalized, self.ssh_host.as_deref())
            .await
    }

    async fn resume_session(&self, session_id: String) -> Result<(), String> {
        let (cwd, startup_mcp_servers) = {
            let mut state = self.state.lock().await;
            state.replaying_history = true;
            state.provider_turn_quarantined = false;
            state.replay_session_id = Some(session_id.clone());
            state.replay_assistant_identity = None;
            state.replay_assistant_text.clear();
            state.replay_assistant_reasoning.clear();
            state.replay_assistant_message_emitted_since_user = false;
            state.replay_error = None;
            (
                state.workspace_root.clone(),
                state.startup_mcp_servers.clone(),
            )
        };

        self.clear_active_stream().await;
        self.emitter.conversation_cleared();
        self.emitter.typing_status_changed(false);

        // kiro-cli-chat doesn't check PID liveness when reading .lock files,
        // so stale locks from dead processes block session/load. Remove the
        // lock file before attempting to load.
        // Agent-specific pre-load cleanup (Kiro's stale `.lock` files).
        let _ = self
            .adapter
            .before_session_load(&session_id, self.ssh_host.as_deref())
            .await;

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
                state.replay_session_id = None;
                state.replay_assistant_identity = None;
                state.replay_assistant_text.clear();
                state.replay_assistant_reasoning.clear();
                state.replay_assistant_message_emitted_since_user = false;
                state.replay_error = None;
                self.emitter.typing_status_changed(false);
                return Err(err);
            }
        };

        if let Err(err) = self.bridge.sync_inbound().await {
            let mut state = self.state.lock().await;
            state.replaying_history = false;
            state.replay_session_id = None;
            state.replay_assistant_identity = None;
            state.replay_assistant_text.clear();
            state.replay_assistant_reasoning.clear();
            state.replay_assistant_message_emitted_since_user = false;
            state.replay_error = None;
            self.emitter.typing_status_changed(false);
            return Err(format!("Failed to finish Kiro session replay: {err}"));
        }

        {
            let mut state = self.state.lock().await;
            if let Some(error) = state.replay_error.take() {
                state.replaying_history = false;
                state.replay_session_id = None;
                state.replay_assistant_identity = None;
                state.replay_assistant_text.clear();
                state.replay_assistant_reasoning.clear();
                state.replay_assistant_message_emitted_since_user = false;
                self.emitter.typing_status_changed(false);
                return Err(error);
            }
            if !state.active_tool_contexts.is_empty() {
                let pending = state
                    .active_tool_contexts
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ");
                state.replaying_history = false;
                state.replay_session_id = None;
                state.replay_assistant_identity = None;
                state.replay_assistant_text.clear();
                state.replay_assistant_reasoning.clear();
                state.replay_assistant_message_emitted_since_user = false;
                state.active_tool_contexts.clear();
                state.tool_call_aliases.clear();
                self.emitter.typing_status_changed(false);
                return Err(format!(
                    "Kiro session replay ended with unresolved tool calls: {pending}"
                ));
            }
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
            self.emitter.session_started(&state.session_id);
        }

        self.flush_replay_assistant_message().await;
        self.state.lock().await.replay_session_id = None;
        self.emitter.typing_status_changed(false);
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
        self.emitter.close("ACP session closed");
    }

    async fn handle_inbound(&self, inbound: AcpInbound) {
        match inbound {
            AcpInbound::Stderr(line) => {
                self.emitter.subprocess_stderr(&line);
            }
            AcpInbound::Closed { exit_code } => {
                let code = if self.shutting_down.load(Ordering::Acquire) {
                    Some(0)
                } else {
                    exit_code
                };
                self.emitter.subprocess_exit(code);
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
                        self.emitter.subprocess_stderr(&format!(
                            "Failed to handle server request '{method}': {err}"
                        ));
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
            "session/update" => {
                tracing::debug!(?params, "ACP session/update notification");
                if !self.accept_replay_notification_session(params).await {
                    return;
                }
                self.handle_standard_update(params).await;
            }
            other => {
                // Anything that isn't the standard notification is only
                // meaningful if this agent's adapter recognizes it.
                let Some(normalized) = self.adapter.normalize_notification(other, params) else {
                    return;
                };
                if !self.accept_replay_notification_session(params).await {
                    return;
                }
                self.handle_normalized_update(normalized.session_update, &normalized.params)
                    .await;
            }
        }
    }

    async fn accept_replay_notification_session(&self, params: &Value) -> bool {
        let error = {
            let state = self.state.lock().await;
            if !state.replaying_history {
                return true;
            }
            let expected = state.replay_session_id.as_deref();
            let actual = params
                .get("sessionId")
                .or_else(|| params.get("session_id"))
                .and_then(Value::as_str);
            match (expected, actual) {
                (Some(expected), Some(actual)) if expected == actual => None,
                (Some(_), Some(_)) => Some(
                    "Kiro session replay received an event for a different session".to_string(),
                ),
                (Some(_), None) => {
                    Some("Kiro session replay event omitted its session identity".to_string())
                }
                (None, _) => {
                    Some("Kiro session replay received an event outside session/load".to_string())
                }
            }
        };

        if let Some(error) = error {
            self.set_replay_error(error).await;
            false
        } else {
            true
        }
    }

    /// Ask the adapter to describe a tool call.
    ///
    /// ACP carries the classification in `kind`; everything else in the
    /// payload is agent-defined, which is why the adapter decides.
    async fn map_tool_request(&self, params: &Value, args: &Value, workspace_root: &str) -> Value {
        let kind = params.get("kind").and_then(Value::as_str).unwrap_or("");
        let mapped = self
            .adapter
            .map_tool_request(kind, args, workspace_root)
            .await;
        if kind == "read" && mapped.get("kind").and_then(Value::as_str) == Some("Other") {
            let tool_call_id = params
                .get("toolCallId")
                .or_else(|| params.get("tool_call_id"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or("<missing>");
            let mut raw_input_keys = args
                .as_object()
                .map(|object| object.keys().cloned().collect::<Vec<_>>())
                .unwrap_or_default();
            raw_input_keys.sort();
            tracing::warn!(
                message = "ACP read normalization degraded to Other",
                agent = self.adapter.display_name(),
                tool_call_id = tool_call_id,
                raw_input_keys = ?raw_input_keys,
            );
        }
        mapped
    }

    /// Handle an update an adapter rewrote into standard terms.
    ///
    /// The payload is already flat (no `update` envelope), so it goes straight
    /// to the shared dispatch.
    async fn handle_normalized_update(&self, update_type: &str, params: &Value) {
        if self.should_drop_quarantined_update(update_type).await {
            return;
        }
        self.dispatch_session_update(update_type, params).await;
    }

    /// A provider error quarantines the rest of the turn. Error updates still
    /// get through, because that is how the quarantine is lifted and reported.
    async fn should_drop_quarantined_update(&self, update_type: &str) -> bool {
        if update_type == "error" {
            return false;
        }
        let state = self.state.lock().await;
        !state.replaying_history && state.provider_turn_quarantined
    }

    /// The one place a `session/update` discriminant is turned into behavior.
    ///
    /// Both the standard notification and anything an adapter normalized land
    /// here, so an agent with a proprietary notification family gets exactly
    /// the same handling as a conforming one.
    async fn dispatch_session_update(&self, update_type: &str, update: &Value) {
        match update_type {
            "agent_message_chunk" => self.handle_agent_message_chunk(update).await,
            "user_message_chunk" => self.handle_user_message_chunk(update).await,
            "agent_thought_chunk" => self.handle_reasoning_chunk(update).await,
            "tool_call" => self.handle_tool_call(update).await,
            "tool_call_update" => self.handle_tool_call_update(update).await,
            "error" => self.handle_error_notification(update).await,
            "plan" => self.handle_plan_update(update),
            // Not part of the standard update family; adapters for agents that
            // signal end-of-turn out of band normalize onto this.
            "turn_end" => {
                if self.state.lock().await.replaying_history {
                    self.flush_replay_assistant_message().await;
                    return;
                }
                self.finalize_active_stream_if_any(Some(update.clone()), true)
                    .await;
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

    async fn handle_standard_update(&self, params: &Value) {
        let update = params.get("update").unwrap_or(params);
        let update_type = update
            .get("sessionUpdate")
            .or_else(|| update.get("session_update"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        if self.should_drop_quarantined_update(update_type).await {
            return;
        }
        self.dispatch_session_update(update_type, update).await;
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
        self.emitter.user_message(&text, None);
        self.state
            .lock()
            .await
            .replay_assistant_message_emitted_since_user = false;
    }

    async fn handle_reasoning_chunk(&self, params: &Value) {
        let delta = extract_text_from_update(params);
        if delta.trim().is_empty() {
            return;
        }

        if self.state.lock().await.replaying_history {
            self.append_replay_assistant_chunk(extract_kiro_chat_message_id(params), &delta, true)
                .await;
            return;
        }

        let response = {
            let mut state = self.state.lock().await;
            if state.replaying_history {
                return;
            }
            let started = state.active_response.is_none();
            let model = state.model.clone().unwrap_or_else(|| "kiro".to_string());
            if started {
                self.emitter.typing_status_changed(true);
                let response = self.emitter.stream_start(Some(&model));
                state.active_response = Some(response);
                state.active_stream_text.clear();
                state.active_stream_tool_calls.clear();
            }
            state.active_response.clone().expect("response just opened")
        };
        self.emitter.stream_reasoning_delta(&response, &delta);
    }

    async fn append_replay_assistant_chunk(
        &self,
        provider_message_id: Option<ChatMessageId>,
        delta: &str,
        reasoning: bool,
    ) {
        let previous = {
            let mut state = self.state.lock().await;
            let active_identity = state.replay_assistant_identity.clone();
            let identity = match active_identity {
                Some(active)
                    if provider_message_id.is_none()
                        || provider_message_id == active.message_id =>
                {
                    active
                }
                _ => KiroReplayMessageIdentity::new(provider_message_id),
            };

            let previous = if state
                .replay_assistant_identity
                .as_ref()
                .is_some_and(|active| active.message_id != identity.message_id)
            {
                state.replay_assistant_identity.take().map(|active| {
                    (
                        active,
                        std::mem::take(&mut state.replay_assistant_text),
                        std::mem::take(&mut state.replay_assistant_reasoning),
                    )
                })
            } else {
                None
            };

            state.replay_assistant_identity = Some(identity);
            if reasoning {
                state.replay_assistant_reasoning.push_str(delta);
            } else {
                state.replay_assistant_text.push_str(delta);
            }
            previous
        };

        if let Some(previous) = previous {
            self.emit_replay_message(Some(previous)).await;
        }
    }

    async fn handle_agent_message_chunk(&self, params: &Value) {
        let raw_delta = extract_text_from_update(params);
        if raw_delta.is_empty() {
            return;
        }
        let delta = self.adapter.sanitize_stream_text(&raw_delta).into_owned();
        if delta.is_empty() {
            return;
        }

        if self.state.lock().await.replaying_history {
            self.append_replay_assistant_chunk(extract_kiro_chat_message_id(params), &delta, false)
                .await;
            return;
        }

        if !has_renderable_stream_text(&delta) {
            let has_active_stream = self.state.lock().await.active_response.is_some();
            if !has_active_stream {
                return;
            }
        }

        let response = {
            let mut state = self.state.lock().await;
            if state.active_response.is_none() {
                let model = state.model.clone().unwrap_or_else(|| "kiro".to_string());
                self.emitter.typing_status_changed(true);
                state.active_response = Some(self.emitter.stream_start(Some(&model)));
                state.active_stream_text.clear();
                state.active_stream_tool_calls.clear();
            }
            state.active_stream_text.push_str(&delta);
            state.active_response.clone().expect("response just opened")
        };
        self.emitter.stream_delta(&response, &delta);
    }

    async fn set_replay_error(&self, message: String) {
        let mut state = self.state.lock().await;
        if state.replay_error.is_none() {
            state.replay_error = Some(message);
        }
    }

    async fn replay_error_is_set(&self) -> bool {
        self.state.lock().await.replay_error.is_some()
    }

    async fn ensure_replay_assistant_message_for_tool(
        &self,
        identity: KiroReplayMessageIdentity,
        tool_call: ToolUseData,
    ) {
        self.flush_replay_assistant_message().await;
        let should_emit = {
            let state = self.state.lock().await;
            state.replaying_history && state.replay_error.is_none()
        };
        if should_emit {
            self.emit_replay_assistant_message(
                identity,
                String::new(),
                String::new(),
                vec![tool_call],
                true,
            )
            .await;
        }
    }

    async fn handle_replay_tool_call(&self, params: &Value) {
        if self.replay_error_is_set().await {
            return;
        }

        let Some(request) = parse_tool_call_request(params) else {
            self.set_replay_error(format!(
                "Kiro session replay contained tool_call without toolCallId: {params}"
            ))
            .await;
            return;
        };

        let raw_tool_call_id = normalize_tool_call_id_fragment(&request.tool_call_id);
        self.append_replay_assistant_chunk(extract_kiro_chat_message_id(params), "", false)
            .await;
        if self.replay_error_is_set().await {
            return;
        }
        let identity = { self.state.lock().await.replay_assistant_identity.clone() };
        let Some(identity) = identity else {
            self.set_replay_error(
                "Kiro replay tool identity was not retained at the decode boundary".to_string(),
            )
            .await;
            return;
        };
        let (workspace_root, is_mcp_tool) = {
            let state = self.state.lock().await;
            (
                state.workspace_root.clone(),
                kiro_is_startup_mcp_tool(&request.tool_name, &state.startup_mcp_servers),
            )
        };
        let tool_type = self
            .map_tool_request(params, &request.args, &workspace_root)
            .await;
        let canonical_id = normalize_tool_call_id_fragment(&raw_tool_call_id);

        {
            let mut state = self.state.lock().await;
            if state.active_tool_contexts.contains_key(&canonical_id) {
                state.replay_error = Some(format!(
                    "Kiro session replay contained duplicate tool_call id {canonical_id}"
                ));
                return;
            }

            state.active_tool_contexts.insert(
                canonical_id.clone(),
                KiroToolContext {
                    tool_name: request.tool_name.clone(),
                    tool_type: tool_type.clone(),
                    is_mcp_tool,
                    request_emitted: true,
                    pending_completion: None,
                },
            );
            state
                .tool_call_aliases
                .insert(tool_alias_raw_key(&raw_tool_call_id), canonical_id.clone());
            if let Some(message_id) = identity.message_id.as_ref() {
                state.tool_call_aliases.insert(
                    tool_alias_message_key(&message_id.0, &raw_tool_call_id),
                    canonical_id.clone(),
                );
            }
        }

        self.ensure_replay_assistant_message_for_tool(
            identity,
            ToolUseData {
                tool_call_id: canonical_id.clone(),
                name: request.tool_name.clone(),
                arguments: public_acp_tool_arguments(&request.args),
                content_offset: Some(0),
            },
        )
        .await;
        if self.replay_error_is_set().await {
            return;
        }

        self.emitter
            .tool_request(&canonical_id, kiro_tool_request_type(tool_type));
    }

    async fn handle_replay_tool_call_update(&self, params: &Value) {
        if self.replay_error_is_set().await {
            return;
        }

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
            self.set_replay_error(format!(
                "Kiro session replay contained tool_call_update for unknown toolCallId: {params}"
            ))
            .await;
            return;
        };
        let Some(mut completion) = parse_tool_call_completion(params, fallback_name) else {
            return;
        };
        completion.tool_call_id = resolved_tool_call_id;

        let completion_to_emit = {
            let mut state = self.state.lock().await;
            let Some(context) = state.active_tool_contexts.get(&completion.tool_call_id) else {
                state.replay_error = Some(format!(
                    "Kiro session replay lost context for tool_call_update id {}",
                    completion.tool_call_id
                ));
                return;
            };

            completion.tool_name = context.tool_name.clone();
            completion.is_mcp_tool = context.is_mcp_tool;
            let tool_result = self
                .adapter
                .map_tool_result(&completion, Some(&context.tool_type));
            let (success, error) = normalize_mapped_tool_outcome(
                &completion.tool_call_id,
                &completion.tool_name,
                &tool_result,
                completion.success,
                completion.error.clone(),
            );
            let output = (
                completion.tool_call_id.clone(),
                completion.tool_name.clone(),
                tool_result,
                success,
                error,
            );

            state.active_tool_contexts.remove(&completion.tool_call_id);
            // Recorded so a live call after replay that reuses a replayed id
            // mints a fresh occurrence instead of colliding with the retired
            // replayed identity.
            state
                .completed_tool_call_ids
                .insert(completion.tool_call_id.clone());
            remove_tool_call_aliases(
                &mut state.tool_call_aliases,
                &completion.tool_call_id,
                raw_tool_call_id.as_deref(),
                message_id.as_deref(),
            );
            output
        };

        let (tool_call_id, _tool_name, tool_result, success, error) = completion_to_emit;
        self.emitter.tool_completed(
            &tool_call_id,
            kiro_tool_execution_outcome(tool_result, success, error),
        );
    }

    async fn handle_tool_call(&self, params: &Value) {
        if self.state.lock().await.replaying_history {
            self.handle_replay_tool_call(params).await;
            return;
        }

        let Some(request) = parse_tool_call_request(params) else {
            self.emitter.subprocess_stderr(&format!(
                "Ignoring ACP tool_call without toolCallId: {params}"
            ));
            return;
        };
        let raw_tool_call_id = normalize_tool_call_id_fragment(&request.tool_call_id);

        let incoming_message_id = extract_kiro_message_id(params);
        let (workspace_root, is_mcp_tool) = {
            let state = self.state.lock().await;
            (
                state.workspace_root.clone(),
                kiro_is_startup_mcp_tool(&request.tool_name, &state.startup_mcp_servers),
            )
        };

        let mut start_event: Option<String> = None;
        let mut declaration: Option<(ToolUseData, String, Value)> = None;
        {
            let mut state = self.state.lock().await;
            let canonical_id = build_canonical_tool_call_id(
                &mut state,
                incoming_message_id.as_deref().unwrap_or_default(),
                &raw_tool_call_id,
            );
            let duplicate_request = state.active_tool_contexts.contains_key(&canonical_id);
            let tool_type = self
                .map_tool_request(params, &request.args, &workspace_root)
                .await;

            let context = state
                .active_tool_contexts
                .entry(canonical_id.clone())
                .or_insert_with(|| KiroToolContext {
                    tool_name: request.tool_name.clone(),
                    tool_type: tool_type.clone(),
                    is_mcp_tool,
                    request_emitted: false,
                    pending_completion: None,
                });
            let prev_tool_type = context.tool_type.clone();
            let request_already_emitted = context.request_emitted;
            context.tool_type = tool_type.clone();
            context.is_mcp_tool |= is_mcp_tool;

            if duplicate_request && request_already_emitted {
                let changed = prev_tool_type != tool_type;
                if changed {
                    // The re-emitted request advances the emitted identity, so
                    // the stored context must advance with it: a titleless
                    // completion later falls back to `context.tool_name`, which
                    // has to name the card the user is actually looking at.
                    // "tool" is the parser's missing-title placeholder and must
                    // not overwrite a real name.
                    if request.tool_name != "tool" {
                        context.tool_name = request.tool_name.clone();
                    }
                }
            }

            state
                .tool_call_aliases
                .insert(tool_alias_raw_key(&raw_tool_call_id), canonical_id.clone());
            if let Some(message_id) = incoming_message_id.as_deref() {
                state.tool_call_aliases.insert(
                    tool_alias_message_key(message_id, &raw_tool_call_id),
                    canonical_id.clone(),
                );
            }

            if !duplicate_request {
                if state.active_response.is_none() {
                    state.active_stream_text.clear();
                    state.active_stream_tool_calls.clear();
                    let model = state.model.clone().unwrap_or_else(|| "kiro".to_string());
                    start_event = Some(model);
                }

                let tool_call_entry = ToolUseData {
                    tool_call_id: canonical_id.clone(),
                    name: request.tool_name.clone(),
                    arguments: public_acp_tool_arguments(&request.args),
                    content_offset: Some(
                        u32::try_from(state.active_stream_text.chars().count()).unwrap_or(u32::MAX),
                    ),
                };
                let already_present = state
                    .active_stream_tool_calls
                    .iter()
                    .any(|call| call.tool_call_id == canonical_id);
                if !already_present {
                    state.active_stream_tool_calls.push(tool_call_entry.clone());
                    declaration = Some((tool_call_entry, canonical_id.clone(), tool_type));
                }
            }
        };

        if let Some(model) = start_event {
            self.emitter.typing_status_changed(true);
            let response = self.emitter.stream_start(Some(&model));
            let mut state = self.state.lock().await;
            state.active_response = Some(response);
        }

        // Declare the call on the response that is still open rather than
        // ending that response to declare it. Kiro delivers every call of a
        // parallel batch before the first result, so ending here would give
        // each one its own chat message; declaring keeps the card immediate,
        // which waiting for the result would not — Kiro sends no in-progress
        // update, only the completion.
        if let Some((declaration, canonical_id, tool_type)) = declaration {
            let response = { self.state.lock().await.active_response.clone() };
            if let Some(response) = response {
                self.emitter
                    .declare_streaming_tools(&response, vec![declaration]);
                self.emitter
                    .tool_request(&canonical_id, kiro_tool_request_type(tool_type));
                let mut state = self.state.lock().await;
                if let Some(context) = state.active_tool_contexts.get_mut(&canonical_id) {
                    context.request_emitted = true;
                }
            }
        }
    }

    async fn handle_tool_call_update(&self, params: &Value) {
        if self.state.lock().await.replaying_history {
            self.handle_replay_tool_call_update(params).await;
            return;
        }

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

        // The first result ends the response that issued the calls. Every call
        // of a batch has arrived by now, so the closing `StreamEnd` carries all
        // of them and the client can tell they came from one response.
        self.finalize_active_stream_if_any(None, false).await;

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
        {
            let mut state = self.state.lock().await;
            if let Some(context) = state.active_tool_contexts.get_mut(&completion.tool_call_id) {
                if let Some(after_contents) = backfilled_after_contents.clone() {
                    let current_after = context
                        .tool_type
                        .get("after")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    if current_after != after_contents
                        && let Some(obj) = context.tool_type.as_object_mut()
                    {
                        obj.insert("after".to_string(), Value::String(after_contents));
                    }
                }

                if completion.tool_name == "tool" {
                    completion.tool_name = context.tool_name.clone();
                }
                completion.is_mcp_tool = context.is_mcp_tool;
                let tool_result = self
                    .adapter
                    .map_tool_result(&completion, Some(&context.tool_type));
                let (success, error) = normalize_mapped_tool_outcome(
                    &completion.tool_call_id,
                    &completion.tool_name,
                    &tool_result,
                    completion.success,
                    completion.error.clone(),
                );
                let pending = PendingToolCompletion {
                    tool_name: completion.tool_name.clone(),
                    tool_result,
                    success,
                    error,
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
                state
                    .completed_tool_call_ids
                    .insert(completion.tool_call_id.clone());
                remove_tool_call_aliases(
                    &mut state.tool_call_aliases,
                    &completion.tool_call_id,
                    raw_tool_call_id.as_deref(),
                    message_id.as_deref(),
                );
            }
        }

        if let Some((tool_call_id, _tool_name, tool_result, success, error)) = emit_completion_now {
            self.emitter.tool_completed(
                &tool_call_id,
                kiro_tool_execution_outcome(tool_result, success, error),
            );
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
                    .unwrap_or("step")
                    .to_string();
                let status = kiro_plan_status_to_task_status(
                    step.get("status").and_then(Value::as_str).unwrap_or(""),
                );

                protocol::Task {
                    id: index as u64 + 1,
                    description,
                    status,
                }
            })
            .collect::<Vec<_>>();

        self.emitter
            .task_update(&protocol::TaskList { title, tasks });
    }

    async fn handle_error_notification(&self, params: &Value) {
        let message = params
            .get("message")
            .or_else(|| params.get("error").and_then(|v| v.get("message")))
            .and_then(Value::as_str)
            .unwrap_or("Kiro error")
            .to_string();

        if self.state.lock().await.replaying_history {
            self.set_replay_error(format!("Kiro session replay failed: {message}"))
                .await;
            return;
        }

        {
            let mut state = self.state.lock().await;
            if state.provider_turn_quarantined {
                return;
            }
            state.provider_turn_quarantined = true;
        }

        self.abort_active_turn(&message).await;
        self.emitter.backend_error(&message);
    }

    async fn emit_replay_message(
        &self,
        replay: Option<(KiroReplayMessageIdentity, String, String)>,
    ) {
        let Some((identity, text, reasoning)) = replay else {
            return;
        };
        let text = text.trim().to_string();
        let reasoning = reasoning.trim().to_string();
        self.emit_replay_assistant_message(identity, text, reasoning, Vec::new(), false)
            .await;
    }

    async fn emit_replay_assistant_message(
        &self,
        identity: KiroReplayMessageIdentity,
        text: String,
        reasoning: String,
        tool_calls: Vec<ToolUseData>,
        allow_empty: bool,
    ) {
        if text.is_empty() && reasoning.is_empty() && !allow_empty {
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
        self.emitter.replay_assistant_message(
            crate::backend::turn_emitter::AssistantMessagePayload {
                message_id: identity.message_id,
                content: text,
                reasoning: (!reasoning.is_empty()).then_some(ReasoningData {
                    text: reasoning,
                    tokens: None,
                    signature: None,
                    blob: None,
                }),
                tool_calls,
                model_info: Some(ModelInfo { model }),
                token_usage: None,
                context_breakdown: None,
                images: Vec::new(),
            },
        );
        let mut state = self.state.lock().await;
        state.replay_assistant_message_emitted_since_user = true;
    }

    async fn flush_replay_assistant_message(&self) {
        let replay = {
            let mut state = self.state.lock().await;
            state.replay_assistant_identity.take().map(|identity| {
                (
                    identity,
                    std::mem::take(&mut state.replay_assistant_text),
                    std::mem::take(&mut state.replay_assistant_reasoning),
                )
            })
        };
        self.emit_replay_message(replay).await;
    }

    async fn finalize_active_stream_if_any(&self, usage: Option<Value>, end_typing: bool) {
        let active = {
            let mut state = self.state.lock().await;
            state.active_response.take().map(|response| {
                (
                    response,
                    std::mem::take(&mut state.active_stream_text),
                    std::mem::take(&mut state.active_stream_tool_calls),
                )
            })
        };

        if let Some((response, text, tool_calls)) = active {
            self.emit_stream_end(response, text, usage, tool_calls, end_typing)
                .await;
        } else if end_typing {
            self.emitter.typing_status_changed(false);
        }
    }

    async fn clear_active_stream(&self) {
        let mut state = self.state.lock().await;
        state.active_response = None;
        state.active_stream_text.clear();
        state.active_stream_tool_calls.clear();
        state.active_tool_contexts.clear();
        state.tool_call_aliases.clear();
    }

    async fn abort_active_turn(&self, message: &str) {
        self.clear_active_stream().await;
        self.emitter.operation_cancelled(message);
    }

    async fn emit_stream_end(
        &self,
        response: ResponseHandle,
        text: String,
        token_usage: Option<Value>,
        tool_calls: Vec<ToolUseData>,
        end_typing: bool,
    ) {
        let cleaned_text = self.adapter.sanitize_stream_text(&text).into_owned();

        let (session_id, model) = {
            let state = self.state.lock().await;
            (
                state.session_id.clone(),
                state.model.clone().unwrap_or_else(|| "kiro".to_string()),
            )
        };
        tracing::debug!(
            session_id,
            text_bytes = cleaned_text.len(),
            tool_call_count = tool_calls.len(),
            "Finalizing Kiro response stream"
        );
        let normalized_usage = normalize_token_usage(token_usage.as_ref());
        let context_breakdown = normalized_usage
            .as_ref()
            .map(estimate_context_breakdown_from_usage)
            .and_then(|value| serde_json::from_value::<ContextBreakdown>(value).ok());
        let message_token_usage = normalized_usage
            .as_ref()
            .map(kiro_message_token_usage)
            .unwrap_or_else(|| {
                MessageTokenUsage::unavailable(TokenUsageUnavailableReason::BackendDidNotReport)
            });
        let tool_calls_for_events = tool_calls.clone();

        self.emitter.stream_end(
            response,
            StreamEndPayload {
                content: cleaned_text,
                model_info: Some(ModelInfo { model }),
                token_usage: Some(message_token_usage),
                reasoning: None,
                tool_calls: tool_calls.clone(),
                context_breakdown,
                images: Vec::new(),
            },
        );
        self.flush_tool_events_after_stream_end(&tool_calls_for_events)
            .await;
        if end_typing {
            self.emitter.typing_status_changed(false);
        }
    }

    async fn flush_tool_events_after_stream_end(&self, tool_calls: &[ToolUseData]) {
        let mut completions_to_emit: Vec<(String, String, Value, bool, Option<String>)> =
            Vec::new();
        let mut requests_to_emit: Vec<(String, String, Value)> = Vec::new();

        {
            let mut state = self.state.lock().await;
            for tool_call in tool_calls {
                let tool_call_id = tool_call.tool_call_id.clone();

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
                state.completed_tool_call_ids.insert(tool_call_id.clone());
                remove_tool_call_aliases(&mut state.tool_call_aliases, tool_call_id, None, None);
            }
        }

        for (tool_call_id, tool_name, tool_type) in requests_to_emit {
            let _ = tool_name;
            self.emitter
                .tool_request(&tool_call_id, kiro_tool_request_type(tool_type));
        }

        for (tool_call_id, _tool_name, tool_result, success, error) in completions_to_emit {
            self.emitter.tool_completed(
                &tool_call_id,
                kiro_tool_execution_outcome(tool_result, success, error),
            );
        }
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
}

fn kiro_plan_status_to_task_status(raw: &str) -> protocol::TaskStatus {
    match map_plan_status(raw) {
        "completed" => protocol::TaskStatus::Completed,
        "in_progress" => protocol::TaskStatus::InProgress,
        "failed" => protocol::TaskStatus::Failed,
        _ => protocol::TaskStatus::Pending,
    }
}

pub(crate) fn resolve_local_kiro_sessions_dir() -> Result<std::path::PathBuf, String> {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map_err(|_| "Could not determine home directory for Kiro sessions".to_string())?;
    Ok(std::path::PathBuf::from(home)
        .join(".kiro")
        .join("sessions")
        .join("cli"))
}

pub(crate) struct KiroSessionRoots {
    pub(crate) session_cwd: String,
    pub(crate) scope_root: String,
}

pub(crate) async fn resolve_kiro_session_roots(
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

pub(crate) async fn ensure_remote_directory(host: &str, dir: &str) -> Result<(), String> {
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

pub(crate) fn join_posix_path(base: &str, suffix: &str) -> String {
    let base = base.trim_end_matches('/');
    let suffix = suffix.trim_start_matches('/');
    if base.is_empty() {
        format!("/{}", suffix)
    } else {
        format!("{base}/{suffix}")
    }
}

pub(crate) fn strip_ansi_and_controls(input: &str) -> String {
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

fn kiro_tool_request_type(value: Value) -> ToolRequestType {
    serde_json::from_value(value.clone()).unwrap_or(ToolRequestType::Other { args: value })
}

fn kiro_tool_execution_outcome(
    result: Value,
    success: bool,
    error: Option<String>,
) -> ToolExecutionOutcome {
    if success {
        let result =
            serde_json::from_value(result.clone()).unwrap_or(ToolExecutionResult::Other { result });
        ToolExecutionOutcome::Succeeded { result }
    } else {
        ToolExecutionOutcome::Failed {
            message: error.unwrap_or_else(|| "Tool execution failed".to_string()),
            details: (!result.is_null()).then(|| result.to_string()),
            normalization_failure: None,
        }
    }
}

fn kiro_message_token_usage(value: &Value) -> MessageTokenUsage {
    let usage = serde_json::from_value::<TokenUsage>(value.clone()).unwrap_or_default();
    MessageTokenUsage::request_and_turn_known(usage.clone(), usage)
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
    state: &mut KiroState,
    _message_id: &str,
    raw_tool_call_id: &str,
) -> String {
    let base = normalize_tool_call_id_fragment(raw_tool_call_id);
    // Progressive `tool_call` frames for a live call keep merging into its
    // context — that refresh flow is intentional.
    if state.active_tool_contexts.contains_key(&base) {
        return base;
    }
    if let Some(canonical) = state.tool_call_aliases.get(&tool_alias_raw_key(&base))
        && state.active_tool_contexts.contains_key(canonical)
    {
        return canonical.clone();
    }
    // The id was already used by a call that completed: this frame starts a
    // new logical call, so mint a distinct occurrence id (mirroring Codex's
    // reused-provider-id disambiguation) rather than merging it into the
    // dead identity, which downstream would treat as a duplicate and drop.
    if state.completed_tool_call_ids.contains(&base) {
        let occurrence = state.tool_call_occurrences.entry(base.clone()).or_insert(1);
        *occurrence = occurrence.saturating_add(1);
        return format!("{base}:occurrence-{occurrence}");
    }
    base
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

pub(crate) fn has_renderable_stream_text(input: &str) -> bool {
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
fn public_acp_tool_arguments(args: &Value) -> Value {
    let mut arguments = args.clone();
    if let Some(arguments) = arguments.as_object_mut() {
        arguments.remove("__tool_use_purpose");
    }
    arguments
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

fn extract_kiro_chat_message_id(value: &Value) -> Option<ChatMessageId> {
    extract_kiro_message_id(value)
        .map(|message_id| message_id.trim().to_string())
        .filter(|message_id| !message_id.is_empty())
        .map(ChatMessageId)
}

fn extract_kiro_tool_call_id(value: &Value) -> Option<String> {
    extract_tool_call_id(value).or_else(|| {
        extract_first_string_deep(value, &["toolCallId", "tool_call_id", "callId", "call_id"])
    })
}

fn normalize_mapped_tool_outcome(
    tool_call_id: &str,
    tool_name: &str,
    tool_result: &Value,
    provider_success: bool,
    provider_error: Option<String>,
) -> (bool, Option<String>) {
    let nonzero_exit = tool_result
        .get("kind")
        .and_then(Value::as_str)
        .filter(|kind| *kind == "RunCommand")
        .and_then(|_| tool_result.get("exit_code"))
        .and_then(Value::as_i64)
        .filter(|exit_code| *exit_code != 0);
    if provider_success && let Some(exit_code) = nonzero_exit {
        tracing::warn!(
            tool_call_id,
            exit_code,
            "ACP provider marked a nonzero command completion successful; normalized to failure"
        );
        return (
            false,
            Some(format!("{tool_name} exited with status {exit_code}")),
        );
    }
    (provider_success, provider_error)
}

/// Maps a Kiro ACP tool completion to Tyde's internal result representation.
/// Uses the ACP `kind` field: "execute" → RunCommand, "edit" → ModifyFile, "read" → ReadFiles.
/// The `rawOutput` for execute completions is: `{"items": [{"Json": {"exit_status": "exit status: N", "stdout": "...", "stderr": "..."}}]}`
/// The `rawOutput` for read completions is: `{"items": [{"Text": "..."}]}`
/// The `rawOutput` for edit completions is: `{"items": [{"Text": ""}]}`
pub(crate) fn map_tool_completion_result(
    completion: &crate::acp::AcpToolCallCompletion,
    request_payload: Option<&Value>,
) -> Value {
    if completion.is_mcp_tool {
        let normalized = normalize_mcp_call_tool_result(&completion.tool_result);
        if normalized.success && !completion.success {
            let canonical = normalized
                .tool_result
                .get("result")
                .cloned()
                .unwrap_or(Value::Null);
            return json!({
                "kind": "Error",
                "short_message": completion.error.clone().unwrap_or_else(|| format!("{} failed", completion.tool_name)),
                "detailed_message": serde_json::to_string_pretty(&canonical)
                    .unwrap_or_else(|_| canonical.to_string()),
            });
        }
        return normalized.tool_result;
    }
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
            eprintln!(
                "TYDE KIRO EDIT COMPLETION request_payload={request_payload:?} result={}",
                completion.tool_result
            );
            let before = request_payload
                .and_then(|payload| payload.get("before"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            let after = request_payload
                .and_then(|payload| payload.get("after"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            let (lines_added, lines_removed) = crate::backend::estimate_line_delta(before, after);
            json!({
                "kind": "ModifyFile",
                "lines_added": lines_added,
                "lines_removed": lines_removed,
            })
        }
        "read" => {
            let other = || {
                json!({
                    "kind": "Other",
                    "result": completion.tool_result,
                })
            };
            let Some(file_paths) = request_payload
                .filter(|payload| payload.get("kind").and_then(Value::as_str) == Some("ReadFiles"))
                .and_then(|payload| payload.get("file_paths"))
                .and_then(Value::as_array)
                .filter(|paths| !paths.is_empty())
                .and_then(|paths| {
                    paths
                        .iter()
                        .map(|path| {
                            path.as_str()
                                .filter(|path| !path.trim().is_empty())
                                .map(str::to_string)
                        })
                        .collect::<Option<Vec<_>>>()
                })
            else {
                return other();
            };
            let Some(texts) = completion
                .tool_result
                .get("items")
                .and_then(Value::as_array)
                .filter(|items| items.len() == file_paths.len())
                .and_then(|items| {
                    items
                        .iter()
                        .map(|item| item.get("Text").and_then(Value::as_str))
                        .collect::<Option<Vec<_>>>()
                })
            else {
                return other();
            };
            let files = file_paths
                .into_iter()
                .zip(texts)
                .map(|(path, text)| json!({ "path": path, "bytes": text.len() as u64 }))
                .collect::<Vec<_>>();
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

pub(crate) fn normalize_token_usage(raw: Option<&Value>) -> Option<Value> {
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

pub(crate) fn estimate_context_breakdown_from_usage(token_usage: &Value) -> Value {
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
            .ok_or_else(|| "ACP agent model entry missing id".to_string())?;
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
        return Err("ACP agent reported no selectable models".to_string());
    }

    Ok(protocol::SessionSettingsSchema {
        backend_kind: protocol::BackendKind::Acp,
        fields: vec![protocol::SessionSettingField {
            key: "model".to_string(),
            label: "Model".to_string(),
            description: None,
            use_slider: false,
            select_options_by_setting: None,
            field_type: protocol::SessionSettingFieldType::Select {
                options,
                default,
                nullable: true,
            },
        }],
    })
}

/// Probes one ACP agent for its session settings schema.
///
/// The schema comes from the agent's own `ModelsList`, so each configured agent
/// has to be probed separately — `agent` selects which one. `None` probes the
/// built-in Kiro agent, optionally with `program_override` pointing at a
/// different binary (used by tests and the Kiro probe-path setting).
pub(crate) async fn probe_session_settings_schema(
    workspace_roots: &[String],
    program_override: Option<String>,
    agent: Option<&protocol::AcpAgentSpec>,
) -> Result<protocol::SessionSettingsSchema, String> {
    let adapter = match agent {
        Some(spec) => adapter_for_spec(spec),
        None => kiro_adapter(program_override),
    };
    let deadline = tokio::time::Instant::now() + KIRO_SCHEMA_PROBE_TIMEOUT;
    let (session, mut raw_events) =
        KiroSession::spawn_schema_probe(workspace_roots, adapter, deadline).await?;
    let handle = session.command_handle();

    let probe_result = await_kiro_stage(Some(deadline), KiroSchemaProbeStage::ModelsList, async {
        handle.execute(SessionCommand::ListModels).await?;
        loop {
            let raw = raw_events
                .recv()
                .await
                .ok_or_else(|| "ACP schema probe ended before ModelsList".to_string())?;
            if raw.get("kind").and_then(Value::as_str) != Some("ModelsList") {
                continue;
            }
            let known_models = raw
                .get("data")
                .and_then(|data| data.get("models"))
                .and_then(Value::as_array)
                .ok_or_else(|| {
                    "ACP schema probe ModelsList response missing data.models array".to_string()
                })?;
            return session_settings_schema_from_known_models(known_models);
        }
    })
    .await;

    tracing::debug!(
        stage = KiroSchemaProbeStage::Shutdown.label(),
        "ACP schema probe stage started"
    );
    let shutdown_result =
        tokio::time::timeout(KIRO_SCHEMA_PROBE_SHUTDOWN_TIMEOUT, session.shutdown())
            .await
            .map_err(|_| {
                format!(
                    "ACP schema probe stage '{}' timed out",
                    KiroSchemaProbeStage::Shutdown.label()
                )
            });
    if shutdown_result.is_ok() {
        tracing::debug!(
            stage = KiroSchemaProbeStage::Shutdown.label(),
            "ACP schema probe stage completed"
        );
    }

    match (probe_result, shutdown_result) {
        (Ok(schema), Ok(())) => Ok(schema),
        (Ok(_), Err(shutdown_error)) => Err(shutdown_error),
        (Err(probe_error), Ok(())) => Err(probe_error),
        (Err(probe_error), Err(shutdown_error)) => {
            tracing::warn!(
                error = %shutdown_error,
                "ACP schema probe cleanup failed after an earlier probe failure"
            );
            Err(probe_error)
        }
    }
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

pub(crate) fn find_in_path(binary: &str) -> Option<String> {
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

pub(crate) fn resolve_kiro_chat_binary() -> String {
    if let Some(path) = find_in_path("kiro-cli-chat") {
        return path;
    }
    if let Some(path) = resolve_sibling_binary("kiro-cli", "kiro-cli-chat") {
        return path;
    }
    "kiro-cli-chat".to_string()
}

pub(crate) fn pick_workspace_root(workspace_roots: &[String]) -> Result<String, String> {
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
        return Err("Kiro backend requires at least one local workspace root".to_string());
    }
    crate::backend::tyde_owned_no_root_cwd("kiro")
}

/// Maps one ACP `SessionInfo` onto Tyde's `BackendSession`.
///
/// `sessionId` is the only required field, so an entry without one is dropped
/// rather than surfaced as an unresumable row. `updatedAt` is ISO 8601 in the
/// spec; the spec carries no created-at or token count, so those stay unset
/// instead of being invented from the timestamp we do have.
///
/// `resumable` is reported as true because `session/list` exists to enumerate
/// sessions for resuming. An agent that lists a session it cannot load is
/// misbehaving, and `session/load` reports that at the point it happens.
fn acp_session_info_to_backend_session(info: &Value) -> Option<BackendSession> {
    let session_id = info
        .get("sessionId")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())?;
    let cwd = info
        .get("cwd")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|cwd| !cwd.is_empty());
    let updated_at_ms = info
        .get("updatedAt")
        .and_then(Value::as_str)
        .and_then(parse_iso8601_to_unix_ms);

    Some(BackendSession {
        id: SessionId(session_id.to_string()),
        backend_kind: BackendKind::Acp,
        workspace_roots: cwd.map(|cwd| vec![cwd.to_string()]).unwrap_or_default(),
        title: info
            .get("title")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|title| !title.is_empty())
            .map(str::to_string),
        token_count: None,
        created_at_ms: None,
        updated_at_ms,
        resumable: true,
    })
}

pub(crate) fn parse_iso8601_to_unix_ms(s: &str) -> Option<u64> {
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

pub(crate) fn unix_now_ms() -> u64 {
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

pub(crate) async fn clear_local_kiro_session_lock(session_id: &str) -> Result<(), String> {
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

pub(crate) async fn clear_remote_kiro_session_lock(
    host: &str,
    session_id: &str,
) -> Result<(), String> {
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

pub(crate) async fn delete_local_kiro_session(session_id: &str) -> Result<(), String> {
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

pub(crate) async fn delete_remote_kiro_session(host: &str, session_id: &str) -> Result<(), String> {
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

pub(crate) async fn load_local_kiro_sessions() -> Result<Vec<(String, Value)>, String> {
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

pub(crate) async fn load_remote_kiro_sessions(host: &str) -> Result<Vec<(String, Value)>, String> {
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

pub(crate) fn extract_session_title(metadata: &Value) -> String {
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

pub(crate) fn extract_session_timestamp(metadata: &Value) -> u64 {
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
    AgentInput, BackendKind, ChatEvent, ChatMessage, MessageSender, SessionId, SessionSettingValue,
    SpawnCostHint, StreamEndData, StreamStartData, StreamTextDeltaData,
};

use crate::backend::{
    Backend, BackendCompactionCapability, BackendCompactionUnavailableReason, BackendSession,
    BackendSpawnConfig, EventStream, empty_session_settings_schema, protocol_images_to_attachments,
    resolve_settings as resolve_backend_settings, session_settings_to_json,
};

const BACKEND_AGENT_NAME: &str = "kiro";

pub struct KiroBackend {
    input_tx: mpsc::UnboundedSender<AgentInput>,
    interrupt_tx: mpsc::UnboundedSender<()>,
    session_id: Arc<std::sync::Mutex<Option<SessionId>>>,
}

struct KiroStartupTaskGuard(Option<tokio::task::AbortHandle>);

impl KiroStartupTaskGuard {
    fn new(task: &tokio::task::JoinHandle<()>) -> Self {
        Self(Some(task.abort_handle()))
    }

    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for KiroStartupTaskGuard {
    fn drop(&mut self) {
        if let Some(abort) = self.0.take() {
            abort.abort();
        }
    }
}

fn kiro_backend_model(cost_hint: Option<SpawnCostHint>) -> Option<&'static str> {
    match cost_hint {
        Some(SpawnCostHint::Low) => Some("claude-haiku-4.5"),
        // Medium is a legacy no-op: spawn on the backend's own defaults.
        Some(SpawnCostHint::Medium) => None,
        Some(SpawnCostHint::High) => Some("claude-sonnet-4.5"),
        None => None,
    }
}

pub(crate) fn kiro_cost_hint_defaults(cost_hint: SpawnCostHint) -> protocol::SessionSettingsValues {
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

impl Backend for KiroBackend {
    fn capabilities() -> tyde_agent_adapter::BackendCapabilities {
        [
            tyde_agent_adapter::BackendCapability::ListSessions,
            tyde_agent_adapter::BackendCapability::ResumeSession,
            tyde_agent_adapter::BackendCapability::Interrupt,
            tyde_agent_adapter::BackendCapability::StartupMcpServers,
            tyde_agent_adapter::BackendCapability::AgentControlTools,
            tyde_agent_adapter::BackendCapability::WorkspaceInstructions,
            tyde_agent_adapter::BackendCapability::Customization,
            tyde_agent_adapter::BackendCapability::GenericModifyFile,
            tyde_agent_adapter::BackendCapability::GenericReadFiles,
            tyde_agent_adapter::BackendCapability::GenericOtherTool,
            tyde_agent_adapter::BackendCapability::RetryTelemetry,
        ]
        .into()
    }

    fn session_settings_schema() -> protocol::SessionSettingsSchema {
        empty_session_settings_schema(BackendKind::Acp)
    }

    fn compaction_capability(&self) -> BackendCompactionCapability {
        BackendCompactionCapability::context_unavailable(
            BackendCompactionUnavailableReason::AdapterHasNoManualTransport,
        )
    }

    async fn spawn(
        workspace_roots: Vec<String>,
        config: BackendSpawnConfig,
        initial_input: protocol::SendMessagePayload,
    ) -> Result<(Self, EventStream), String> {
        let initial_message = initial_input.message;
        let initial_images = protocol_images_to_attachments(initial_input.images);
        let (input_tx, mut input_rx) = mpsc::unbounded_channel::<AgentInput>();
        let (interrupt_tx, mut interrupt_rx) = mpsc::unbounded_channel::<()>();
        let (events_tx, events_rx) = mpsc::unbounded_channel::<ChatEvent>();
        let events_tx_task = events_tx.clone();
        let session_id = Arc::new(std::sync::Mutex::new(None));
        let session_id_task = Arc::clone(&session_id);
        let (ready_tx, ready_rx) = oneshot::channel::<Result<(), String>>();

        let startup_task = tokio::spawn(async move {
            let mut ready_tx: Option<oneshot::Sender<Result<(), String>>> = Some(ready_tx);
            let combined_instructions =
                render_combined_spawn_instructions(&config.resolved_spawn_config);
            let (session, mut raw_events) = match KiroSession::spawn_for_agent(
                &workspace_roots,
                config.acp_agent.as_ref(),
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
                        && events_tx_forward.send(event).is_err()
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

        let mut startup_guard = KiroStartupTaskGuard::new(&startup_task);
        match ready_rx.await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => return Err(err),
            Err(_) => return Err("Kiro spawn initialization task ended early".to_string()),
        }
        startup_guard.disarm();

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
        let (input_tx, mut input_rx) = mpsc::unbounded_channel::<AgentInput>();
        let (interrupt_tx, mut interrupt_rx) = mpsc::unbounded_channel::<()>();
        let (events_tx, events_rx) = mpsc::unbounded_channel::<ChatEvent>();
        let (resume_replay_complete_tx, resume_replay_complete_rx) =
            tokio::sync::oneshot::channel();
        let events_tx_task = events_tx.clone();
        let known_session_id = Arc::new(std::sync::Mutex::new(Some(session_id.clone())));
        let known_session_id_task = Arc::clone(&known_session_id);
        let (ready_tx, ready_rx) = oneshot::channel::<Result<(), String>>();

        let startup_task = tokio::spawn(async move {
            let mut ready_tx: Option<oneshot::Sender<Result<(), String>>> = Some(ready_tx);
            let combined_instructions =
                render_combined_spawn_instructions(&config.resolved_spawn_config);
            let (session, mut raw_events) = match KiroSession::spawn_for_agent(
                &workspace_roots,
                config.acp_agent.as_ref(),
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
                    if let Some(tx) = ready_tx.take() {
                        let _ = tx.send(Err(format!("Failed to spawn Kiro resume session: {err}")));
                    }
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
                if let Some(tx) = ready_tx.take() {
                    let _ = tx.send(Err(format!("Failed to resume Kiro session: {err}")));
                }
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
                if let Some(tx) = ready_tx.take() {
                    let _ = tx.send(Err(format!(
                        "Failed to configure resumed Kiro session: {err}"
                    )));
                }
                session.shutdown().await;
                return;
            }
            while let Ok(raw) = raw_events.try_recv() {
                if let Some(event) = map_kiro_value_to_chat_event(&raw)
                    && events_tx_task.send(event).is_err()
                {
                    session.shutdown().await;
                    return;
                }
            }
            let _ = resume_replay_complete_tx.send(());

            if let Some(tx) = ready_tx.take() {
                let _ = tx.send(Ok(()));
            }

            let events_tx_forward = events_tx_task.clone();
            let forward_task = tokio::spawn(async move {
                while let Some(raw) = raw_events.recv().await {
                    if let Some(event) = map_kiro_value_to_chat_event(&raw)
                        && events_tx_forward.send(event).is_err()
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

        let mut startup_guard = KiroStartupTaskGuard::new(&startup_task);
        match ready_rx.await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => return Err(err),
            Err(_) => return Err("Kiro resume initialization task ended early".to_string()),
        }
        startup_guard.disarm();

        Ok((
            Self {
                input_tx,
                interrupt_tx,
                session_id: known_session_id,
            },
            EventStream::new_with_resume_replay_barrier(events_rx, resume_replay_complete_rx),
        ))
    }

    async fn fork(
        _workspace_roots: Vec<String>,
        _config: BackendSpawnConfig,
        _from_session_id: SessionId,
        _initial_input: protocol::SendMessagePayload,
    ) -> Result<(Self, EventStream), BackendStartupError> {
        Err(BackendStartupError::unsupported(
            backend_fork_unsupported_message(BackendKind::Acp),
        ))
    }

    /// `Backend::list_sessions` is static, so there is no adapter instance to
    /// ask and no way to know which configured agent the caller meant. It
    /// therefore lists the built-in Kiro agent's sessions, which is what this
    /// did when Kiro was the only ACP agent. Per-agent listing needs
    /// `Backend::list_sessions` to carry the agent, which is a wider change to
    /// the backend trait than this refactor makes; the instance-level
    /// `list_sessions` above is already adapter-driven and is the path a
    /// running session uses.
    async fn list_sessions() -> Result<Vec<BackendSession>, String> {
        let mut sessions = kiro_adapter(None).list_sessions(None).await?;
        sessions.sort_by_key(|session| std::cmp::Reverse(session.updated_at_ms));
        Ok(sessions)
    }

    async fn send(&self, input: AgentInput) -> bool {
        match input {
            input @ AgentInput::SendMessage(_) | input @ AgentInput::UpdateSessionSettings(_) => {
                self.input_tx.send(input).is_ok()
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

    async fn interrupt(&self) -> bool {
        self.interrupt_tx.send(()).is_ok()
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
            Some(ChatEvent::StreamDelta(StreamTextDeltaData { text }))
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
                    message_id: msg
                        .get("message_id")
                        .or_else(|| msg.get("messageId"))
                        .and_then(Value::as_str)
                        .map(|message_id| ChatMessageId(message_id.to_string())),
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
