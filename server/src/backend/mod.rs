pub mod acp;
/// Transitional alias while the ACP session lifecycle is being generalized:
/// the generic backend still carries Kiro's type names. Removed once the
/// lifecycle no longer names a specific agent.
pub use acp::backend as kiro;
pub mod agent_control_progress;
pub mod antigravity;
pub mod claude;
pub mod claude_skills;
pub mod codex;
// Trait-facing compaction types are nominally public inside this crate-private
// module so the public Backend trait does not export the internal contract.
pub(crate) mod compaction;
pub mod hermes;
pub mod hermes_config;
pub mod mock;
pub mod setup;
pub mod skill_projection;
pub mod subprocess;
pub mod turn_emitter;
pub mod tycode;
pub mod tycode_config;

use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
    sync::Arc,
};

use protocol::{
    AgentErrorCode, AgentInput, BackendAccessMode, BackendConfigFieldType, BackendConfigSchema,
    BackendConfigValues, BackendKind, ChatEvent, CustomAgentId, ImageData, ModelRequestTokenUsage,
    SendMessagePayload, SessionId, SessionSettingField, SessionSettingFieldType,
    SessionSettingValue, SessionSettingsSchema, SessionSettingsValues, SpawnCostHint,
};
use serde_json::Value;
use settings_model::BackendTierConfig;
use tokio::sync::{mpsc, oneshot};
use tyde_agent_adapter::BackendCapabilities;

use self::subprocess::ImageAttachment;
use crate::agent::customization::ResolvedSpawnConfig;

pub(crate) use compaction::{
    BackendAcceptedCompaction, BackendBindingPrepareError, BackendBindingReadyEvidence,
    BackendCompactionAvailability, BackendCompactionCapability,
    BackendCompactionCapabilityEvidence, BackendCompactionCoordinator,
    BackendCompactionDeferredReason, BackendCompactionEvent, BackendCompactionFailure,
    BackendCompactionFailureKind, BackendCompactionMechanism, BackendCompactionNotDispatchedReason,
    BackendCompactionObservationSource, BackendCompactionProgress, BackendCompactionResult,
    BackendCompactionSuccess, BackendCompactionTerminalEvidence,
    BackendCompactionUnavailableReason, BackendCompactionUnknownReason, BackendCompactionUserFocus,
    BackendCompactionUserFocusProvenance, BackendContextSeed, BackendObservedCompaction,
    PostCompactionTokenCount,
};
pub use compaction::{
    BackendCompactionDispatchState, BackendCompactionMutationState, BackendCompactionRequest,
    BackendCompactionStart,
};

pub(crate) const READ_ONLY_ACCESS_MODE_INSTRUCTIONS: &str = concat!(
    "Backend access mode is read-only (best effort). Treat the workspace as ",
    "read-only: do not create, edit, or delete files, and do not run commands ",
    "that modify files, processes, or external state. You MAY freely inspect ",
    "anything — read files, list directories, and run read-only shell commands ",
    "such as `git status`/`log`/`diff`, `grep`/`rg`, `cat`, `ls`, and ",
    "`find` — to investigate the code. Prefer read/inspection tools; do not ",
    "use write/edit/apply-patch tools. Agent orchestration, including spawning ",
    "and messaging other agents, is allowed."
);

/// Private bridge field used to carry the authoritative MCP result through
/// providers that otherwise flatten `CallToolResult` before reporting it.
pub(crate) const TYDE_MCP_RESULT_ENVELOPE_KEY: &str =
    "__tyde_internal_mcp_call_tool_result_v1_8d6f0f6b9f4c4e1a";

pub(crate) struct NormalizedMcpToolResult {
    pub tool_result: Value,
    pub success: bool,
    pub error: Option<String>,
}

/// Projects an authoritative MCP `CallToolResult` onto Tyde's public tool
/// completion contract. Callers must establish MCP identity before invoking
/// this function; arbitrary opaque tools are not MCP results.
pub(crate) fn normalize_mcp_call_tool_result(raw: &Value) -> NormalizedMcpToolResult {
    let source = find_mcp_call_tool_result(raw).unwrap_or(raw);
    let content = match source.get("content") {
        Some(Value::Array(content)) => Value::Array(content.clone()),
        Some(Value::String(text)) => serde_json::json!([{
            "type": "text",
            "text": text,
        }]),
        Some(other) if !other.is_null() => serde_json::json!([{
            "type": "text",
            "text": other.to_string(),
        }]),
        _ if source.get("result").and_then(Value::as_str).is_some() => serde_json::json!([{
            "type": "text",
            "text": source.get("result").and_then(Value::as_str).unwrap_or_default(),
        }]),
        _ => match source {
            Value::String(text) => serde_json::json!([{
                "type": "text",
                "text": text,
            }]),
            _ => Value::Array(Vec::new()),
        },
    };
    let is_error = source
        .get("isError")
        .or_else(|| source.get("is_error"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut canonical = serde_json::Map::new();
    canonical.insert("content".to_owned(), content);
    canonical.insert("isError".to_owned(), Value::Bool(is_error));
    for key in ["structuredContent", "_meta"] {
        if let Some(value) = source.get(key).filter(|value| !value.is_null()) {
            canonical.insert(key.to_owned(), value.clone());
        }
    }
    let canonical = Value::Object(canonical);
    if is_error {
        let detail =
            serde_json::to_string_pretty(&canonical).unwrap_or_else(|_| canonical.to_string());
        let short = canonical
            .get("content")
            .and_then(Value::as_array)
            .and_then(|content| content.iter().find_map(|part| part.get("text")))
            .and_then(Value::as_str)
            .and_then(|text| text.lines().find(|line| !line.trim().is_empty()))
            .map(|line| line.trim().chars().take(140).collect::<String>())
            .unwrap_or_else(|| "MCP tool execution failed".to_owned());
        NormalizedMcpToolResult {
            tool_result: serde_json::json!({
                "kind": "Error",
                "short_message": short,
                "detailed_message": detail,
            }),
            success: false,
            error: Some("MCP tool returned isError: true".to_owned()),
        }
    } else {
        NormalizedMcpToolResult {
            tool_result: serde_json::json!({
                "kind": "Other",
                "result": canonical,
            }),
            success: true,
            error: None,
        }
    }
}

fn find_mcp_call_tool_result(value: &Value) -> Option<&Value> {
    if let Some(envelope) = value
        .get("structuredContent")
        .and_then(|structured| structured.get(TYDE_MCP_RESULT_ENVELOPE_KEY))
        .filter(|candidate| is_mcp_call_tool_result(candidate))
    {
        return Some(envelope);
    }
    if is_mcp_call_tool_result(value) {
        return Some(value);
    }
    if let Some(result) = value.get("result") {
        if let Some(envelope) = result
            .get("structuredContent")
            .and_then(|structured| structured.get(TYDE_MCP_RESULT_ENVELOPE_KEY))
            .filter(|candidate| is_mcp_call_tool_result(candidate))
        {
            return Some(envelope);
        }
        if is_mcp_call_tool_result(result) {
            return Some(result);
        }
    }
    value
        .get("items")
        .and_then(Value::as_array)
        .and_then(|items| items.first())
        .and_then(|item| item.get("Json"))
        .filter(|candidate| is_mcp_call_tool_result(candidate))
}

fn is_mcp_call_tool_result(value: &Value) -> bool {
    value
        .get("content")
        .is_some_and(|content| content.is_array() || content.is_string())
        && value
            .get("isError")
            .or_else(|| value.get("is_error"))
            .is_none_or(Value::is_boolean)
}

pub(crate) fn protocol_images_to_attachments(
    images: Option<Vec<ImageData>>,
) -> Option<Vec<ImageAttachment>> {
    let attachments = images
        .unwrap_or_default()
        .into_iter()
        .enumerate()
        .map(|(index, image)| ImageAttachment {
            data: image.data,
            media_type: image.media_type,
            name: format!("image-{}", index + 1),
            size: 0,
        })
        .collect::<Vec<_>>();

    if attachments.is_empty() {
        None
    } else {
        Some(attachments)
    }
}

pub(crate) fn tyde_owned_no_root_cwd(backend: &str) -> Result<String, String> {
    let dir = crate::paths::home_dir()?
        .join(".tyde")
        .join(backend)
        .join("no-root");
    std::fs::create_dir_all(&dir).map_err(|err| {
        format!(
            "Failed to create Tyde-owned no-root cwd '{}': {err}",
            dir.display()
        )
    })?;
    Ok(dir.to_string_lossy().to_string())
}

#[derive(Debug, Clone)]
pub enum SessionCommand {
    SendMessage {
        message: String,
        images: Option<Vec<ImageAttachment>>,
    },
    CancelConversation,
    GetSettings,
    ListSessions,
    ResumeSession {
        session_id: String,
    },
    DeleteSession {
        session_id: String,
    },
    ListProfiles,
    SwitchProfile {
        profile_name: String,
    },
    GetModuleSchemas,
    ListModels,
    UpdateSettings {
        settings: Value,
        persist: bool,
    },
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum StartupMcpTransport {
    Http {
        url: String,
        headers: HashMap<String, String>,
        bearer_token_env_var: Option<String>,
    },
    Stdio {
        command: String,
        args: Vec<String>,
        env: HashMap<String, String>,
    },
}

#[derive(Debug, Clone)]
pub struct StartupMcpServer {
    pub name: String,
    pub supports_parallel_tool_calls: bool,
    pub transport: StartupMcpTransport,
}

pub(crate) async fn validate_startup_mcp_configuration(
    servers: &[StartupMcpServer],
) -> Result<(), String> {
    for server in servers {
        match &server.transport {
            StartupMcpTransport::Stdio { command, .. } => {
                if command.trim().is_empty() {
                    return Err(format!(
                        "MCP server '{}' has an empty stdio command",
                        server.name
                    ));
                }
            }
            StartupMcpTransport::Http {
                headers,
                bearer_token_env_var,
                ..
            } => {
                validate_http_mcp_configuration(server, headers, bearer_token_env_var.as_deref())?;
            }
        }
    }
    Ok(())
}

fn validate_http_mcp_configuration(
    server: &StartupMcpServer,
    headers: &HashMap<String, String>,
    bearer_token_env_var: Option<&str>,
) -> Result<(), String> {
    for (name, value) in headers {
        reqwest::header::HeaderName::from_bytes(name.as_bytes()).map_err(|error| {
            format!(
                "MCP server '{}' has invalid HTTP header name '{name}': {error}",
                server.name
            )
        })?;
        reqwest::header::HeaderValue::from_str(value).map_err(|error| {
            format!(
                "MCP server '{}' has an invalid HTTP header value: {error}",
                server.name
            )
        })?;
    }
    if let Some(variable) = bearer_token_env_var
        .map(str::trim)
        .filter(|variable| !variable.is_empty())
    {
        let token = std::env::var(variable).map_err(|error| {
            format!(
                "MCP server '{}' bearer token environment variable '{variable}' is unavailable: {error}",
                server.name
            )
        })?;
        reqwest::header::HeaderValue::from_str(&format!("Bearer {token}")).map_err(|error| {
            format!(
                "MCP server '{}' bearer token is not a valid HTTP header value: {error}",
                server.name
            )
        })?;
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BackendExecutionMode {
    #[default]
    Agent,
    InferenceOnly,
}

#[derive(Debug, Clone)]
pub struct AgentIdentity {
    pub id: String,
    pub description: String,
    pub instructions: String,
}

#[derive(Debug, Clone)]
pub struct BackendSession {
    pub id: SessionId,
    pub backend_kind: BackendKind,
    pub workspace_roots: Vec<String>,
    pub title: Option<String>,
    pub token_count: Option<u64>,
    pub created_at_ms: Option<u64>,
    pub updated_at_ms: Option<u64>,
    pub resumable: bool,
}

#[derive(Debug, Clone, Default)]
pub struct BackendSpawnConfig {
    pub execution_mode: BackendExecutionMode,
    pub cost_hint: Option<SpawnCostHint>,
    pub custom_agent_id: Option<CustomAgentId>,
    pub startup_mcp_servers: Vec<StartupMcpServer>,
    pub session_settings: Option<SessionSettingsValues>,
    /// Installed provider version reported by the host's setup probe.
    pub provider_version: Option<String>,
    /// Resolved Antigravity conversation store used by both ordinary and
    /// prepared replacement bindings.
    pub antigravity_conversations_dir: Option<PathBuf>,
    /// Host-level deep configuration for this backend (see
    /// [`protocol::BackendConfigSchema`]). Empty when unconfigured.
    pub backend_config: BackendConfigValues,
    /// Which ACP agent this session runs, resolved from its launch profile.
    /// Only meaningful for [`BackendKind::Acp`]; `None` there means the
    /// built-in Kiro agent, which keeps sessions recorded before ACP profiles
    /// existed working.
    pub acp_agent: Option<protocol::AcpAgentSpec>,
    pub resolved_spawn_config: ResolvedSpawnConfig,
}

/// Output stream from a backend session. The agent actor receives both chat
/// events and backend-only accounting events while independently sending
/// AgentInput through the Backend handle.
#[derive(Debug, Clone)]
pub(crate) enum BackendEvent {
    Chat(ChatEvent),
    ModelRequestTokenUsage(ModelRequestTokenUsage),
    Compaction(BackendCompactionEvent),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BackendTranscriptEventMetadata {
    /// Both fields are set only when the adapter can attest an event to an
    /// exact provider item. Provider-internal artifacts must be filtered
    /// before they enter EventStream rather than labeled as public history.
    pub provider_session_id: Option<SessionId>,
    pub provider_event_id: Option<String>,
}

impl BackendTranscriptEventMetadata {
    fn visible_without_provider_identity() -> Self {
        Self {
            provider_session_id: None,
            provider_event_id: None,
        }
    }

    pub(crate) fn visible_provider_event(
        provider_session_id: SessionId,
        provider_event_id: String,
    ) -> Self {
        Self {
            provider_session_id: Some(provider_session_id),
            provider_event_id: Some(provider_event_id),
        }
    }
}

type BackendTranscriptMetadataProjector =
    Arc<dyn Fn(&ChatEvent) -> BackendTranscriptEventMetadata + Send + Sync>;

enum EventStreamReceiver {
    Chat(mpsc::UnboundedReceiver<ChatEvent>),
    Backend(mpsc::UnboundedReceiver<BackendEvent>),
}

pub struct EventStream {
    rx: EventStreamReceiver,
    buffered: VecDeque<BackendEvent>,
    resume_replay_complete: Option<oneshot::Receiver<()>>,
    transcript_metadata_projector: Option<BackendTranscriptMetadataProjector>,
}

impl EventStream {
    pub fn new(rx: mpsc::UnboundedReceiver<ChatEvent>) -> Self {
        Self {
            rx: EventStreamReceiver::Chat(rx),
            buffered: VecDeque::new(),
            resume_replay_complete: None,
            transcript_metadata_projector: None,
        }
    }

    pub(crate) fn new_backend(rx: mpsc::UnboundedReceiver<BackendEvent>) -> Self {
        Self {
            rx: EventStreamReceiver::Backend(rx),
            buffered: VecDeque::new(),
            resume_replay_complete: None,
            transcript_metadata_projector: None,
        }
    }

    pub(crate) fn new_backend_with_transcript_metadata(
        rx: mpsc::UnboundedReceiver<BackendEvent>,
        projector: impl Fn(&ChatEvent) -> BackendTranscriptEventMetadata + Send + Sync + 'static,
    ) -> Self {
        Self {
            rx: EventStreamReceiver::Backend(rx),
            buffered: VecDeque::new(),
            resume_replay_complete: None,
            transcript_metadata_projector: Some(Arc::new(projector)),
        }
    }

    pub fn new_with_resume_replay_barrier(
        rx: mpsc::UnboundedReceiver<ChatEvent>,
        resume_replay_complete: oneshot::Receiver<()>,
    ) -> Self {
        Self {
            rx: EventStreamReceiver::Chat(rx),
            buffered: VecDeque::new(),
            resume_replay_complete: Some(resume_replay_complete),
            transcript_metadata_projector: None,
        }
    }

    pub(crate) fn new_backend_with_resume_replay_barrier(
        rx: mpsc::UnboundedReceiver<BackendEvent>,
        resume_replay_complete: oneshot::Receiver<()>,
    ) -> Self {
        Self {
            rx: EventStreamReceiver::Backend(rx),
            buffered: VecDeque::new(),
            resume_replay_complete: Some(resume_replay_complete),
            transcript_metadata_projector: None,
        }
    }

    pub(crate) fn new_backend_with_resume_replay_barrier_and_transcript_metadata(
        rx: mpsc::UnboundedReceiver<BackendEvent>,
        resume_replay_complete: oneshot::Receiver<()>,
        projector: impl Fn(&ChatEvent) -> BackendTranscriptEventMetadata + Send + Sync + 'static,
    ) -> Self {
        Self {
            rx: EventStreamReceiver::Backend(rx),
            buffered: VecDeque::new(),
            resume_replay_complete: Some(resume_replay_complete),
            transcript_metadata_projector: Some(Arc::new(projector)),
        }
    }

    /// Receive the next ChatEvent from the backend.
    /// Returns None when the backend has terminated.
    pub async fn recv(&mut self) -> Option<ChatEvent> {
        loop {
            match self.recv_backend().await? {
                BackendEvent::Chat(event) => return Some(event),
                BackendEvent::ModelRequestTokenUsage(_) | BackendEvent::Compaction(_) => {}
            }
        }
    }

    /// Receive the next normalized event for adapter conformance testing.
    pub async fn recv_observation(&mut self) -> Option<tyde_agent_adapter::BackendObservation> {
        self.recv_backend().await.map(|event| match event {
            BackendEvent::Chat(event) => tyde_agent_adapter::BackendObservation::Chat(event),
            BackendEvent::ModelRequestTokenUsage(usage) => {
                tyde_agent_adapter::BackendObservation::ModelRequestTokenUsage(usage)
            }
            BackendEvent::Compaction(event) => {
                tyde_agent_adapter::BackendObservation::Compaction(match event {
                    BackendCompactionEvent::Progress(progress) => {
                        tyde_agent_adapter::BackendCompactionObservation::Progress {
                            operation_id: progress.operation_id,
                            stage: progress.stage,
                            elapsed_ms: progress.elapsed_ms,
                        }
                    }
                    BackendCompactionEvent::Observed(observed) => {
                        tyde_agent_adapter::BackendCompactionObservation::Observed {
                            observation_id: observed.observation_id,
                            trigger: observed.trigger,
                            method: observed.method,
                            provider_session_id: observed.provider_session_id,
                            metrics: observed.metrics,
                        }
                    }
                })
            }
        })
    }

    /// Non-blocking receive used to drain already-buffered backend events
    /// (e.g. queued resume-replay events) without awaiting new ones.
    pub fn try_recv(&mut self) -> Result<ChatEvent, mpsc::error::TryRecvError> {
        loop {
            match self.try_recv_backend()? {
                BackendEvent::Chat(event) => return Ok(event),
                BackendEvent::ModelRequestTokenUsage(_) | BackendEvent::Compaction(_) => {}
            }
        }
    }

    pub(crate) async fn recv_backend(&mut self) -> Option<BackendEvent> {
        if let Some(event) = self.buffered.pop_front() {
            return Some(event);
        }
        match &mut self.rx {
            EventStreamReceiver::Chat(rx) => rx.recv().await.map(BackendEvent::Chat),
            EventStreamReceiver::Backend(rx) => rx.recv().await,
        }
    }

    pub(crate) fn try_recv_backend(&mut self) -> Result<BackendEvent, mpsc::error::TryRecvError> {
        if let Some(event) = self.buffered.pop_front() {
            return Ok(event);
        }
        match &mut self.rx {
            EventStreamReceiver::Chat(rx) => rx.try_recv().map(BackendEvent::Chat),
            EventStreamReceiver::Backend(rx) => rx.try_recv(),
        }
    }

    pub(crate) fn restore_backend_events(
        &mut self,
        events: impl IntoIterator<Item = BackendEvent>,
    ) {
        let restored = events.into_iter().collect::<Vec<_>>();
        for event in restored.into_iter().rev() {
            self.buffered.push_front(event);
        }
    }

    pub(crate) fn transcript_metadata(&self, event: &ChatEvent) -> BackendTranscriptEventMetadata {
        let metadata = self.transcript_metadata_projector.as_ref().map_or_else(
            BackendTranscriptEventMetadata::visible_without_provider_identity,
            |f| f(event),
        );
        debug_assert_eq!(
            metadata.provider_session_id.is_some(),
            metadata.provider_event_id.is_some(),
            "backend transcript provider identity must be complete"
        );
        metadata
    }

    pub fn take_resume_replay_complete(&mut self) -> Option<oneshot::Receiver<()>> {
        self.resume_replay_complete.take()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendStartupError {
    pub code: AgentErrorCode,
    pub message: String,
}

impl BackendStartupError {
    pub fn backend_failed(message: impl Into<String>) -> Self {
        Self {
            code: AgentErrorCode::BackendFailed,
            message: message.into(),
        }
    }

    pub fn unsupported(message: impl Into<String>) -> Self {
        Self {
            code: AgentErrorCode::Unsupported,
            message: message.into(),
        }
    }
}

impl From<String> for BackendStartupError {
    fn from(message: String) -> Self {
        Self::backend_failed(message)
    }
}

impl From<&str> for BackendStartupError {
    fn from(message: &str) -> Self {
        Self::backend_failed(message)
    }
}

/// Outcome of handing an input to a backend via `Backend::send_with_outcome`.
#[derive(Debug)]
pub enum SendOutcome {
    /// The backend accepted the input.
    Accepted,
    /// The backend already has an active turn (e.g. it resumed on its own
    /// initiative after a sub-agent wakeup) and cannot start a user turn
    /// right now. The input is handed back so the caller can queue it and
    /// retry once the backend goes idle; a backend must never report Busy
    /// after consuming the input.
    Busy(AgentInput),
    /// The backend has terminated and can't accept input.
    Closed,
}

/// A coding agent backend session handle.
///
/// Created via `Backend::spawn()` which returns `(Self, EventStream)`.
/// The handle is used to send input; the EventStream is used to read output.
/// Backends are not object-safe — the agent actor knows the concrete type.
pub trait Backend: Send + Sync + 'static {
    /// Behaviors every supported runtime of this adapter promises to provide.
    fn capabilities() -> BackendCapabilities
    where
        Self: Sized;

    fn session_settings_schema() -> SessionSettingsSchema
    where
        Self: Sized;

    /// Optional host-level deep configuration schema for this backend, rendered
    /// in the settings panel (distinct from the per-session settings bar).
    /// Backends without deep configuration return `None` (the default).
    fn backend_config_schema() -> Option<BackendConfigSchema>
    where
        Self: Sized,
    {
        None
    }

    /// Create a new backend session.
    /// Returns a handle to send input and an EventStream to read output.
    /// The backend must start the session with `initial_input` and know the
    /// protocol-visible session ID before returning.
    fn spawn(
        workspace_roots: Vec<String>,
        config: BackendSpawnConfig,
        initial_input: SendMessagePayload,
    ) -> impl std::future::Future<Output = Result<(Self, EventStream), String>> + Send
    where
        Self: Sized;

    /// Resume an existing backend session.
    fn resume(
        workspace_roots: Vec<String>,
        config: BackendSpawnConfig,
        session_id: SessionId,
    ) -> impl std::future::Future<Output = Result<(Self, EventStream), String>> + Send
    where
        Self: Sized;

    /// Fork an existing backend-native session into a fresh independent
    /// backend-native session, then start it with `initial_input`.
    fn fork(
        workspace_roots: Vec<String>,
        config: BackendSpawnConfig,
        from_session_id: SessionId,
        initial_input: SendMessagePayload,
    ) -> impl std::future::Future<Output = Result<(Self, EventStream), BackendStartupError>> + Send
    where
        Self: Sized;

    /// Enumerate resumable sessions known to this backend.
    fn list_sessions()
    -> impl std::future::Future<Output = Result<Vec<BackendSession>, String>> + Send
    where
        Self: Sized;

    /// Return the backend-native session ID for this live handle.
    fn session_id(&self) -> SessionId;

    /// Send an input event to the backend.
    /// Returns false if the backend has terminated and can't accept input.
    fn send(&self, input: AgentInput) -> impl std::future::Future<Output = bool> + Send;

    /// Send an input event and report whether the backend accepted it, is
    /// busy with a turn it started on its own (handing the input back for
    /// the caller to queue), or has terminated.
    ///
    /// The default forwards to `send`: backends that can only start turns in
    /// response to caller input are never busy at send time, because the
    /// caller serializes sends against the idle/typing lifecycle. Backends
    /// that can open turns on their own initiative (or otherwise reject a
    /// send while working) must override this instead of dropping the input.
    ///
    /// Contract: message sends must be serialized by a single caller (the
    /// agent actor). Admission checks in overrides may be check-then-forward,
    /// which is only race-free while `Accepted` results cannot be issued
    /// concurrently for the same backend.
    fn send_with_outcome(
        &self,
        input: AgentInput,
    ) -> impl std::future::Future<Output = SendOutcome> + Send {
        async move {
            if self.send(input).await {
                SendOutcome::Accepted
            } else {
                SendOutcome::Closed
            }
        }
    }

    fn compaction_capability(&self) -> BackendCompactionCapability {
        BackendCompactionCapability::legacy_unavailable(
            BackendCompactionUnavailableReason::AdapterHasNoManualTransport,
        )
    }

    fn begin_compaction(
        &self,
        _request: BackendCompactionRequest,
    ) -> impl std::future::Future<Output = BackendCompactionStart> + Send {
        async {
            BackendCompactionStart::NotDispatched {
                reason: BackendCompactionNotDispatchedReason::NativeUnavailable(
                    BackendCompactionUnavailableReason::AdapterHasNoManualTransport,
                ),
                fallback_safe: true,
            }
        }
    }

    fn update_session_settings(
        &mut self,
        payload: protocol::SetSessionSettingsPayload,
    ) -> impl std::future::Future<Output = Result<(), String>> + Send {
        async move {
            self.send(AgentInput::UpdateSessionSettings(payload))
                .await
                .then_some(())
                .ok_or_else(|| "backend terminated before applying session settings".to_owned())
        }
    }

    /// Test support: the live [`mock::MockControl`] handle when this backend
    /// is the scriptable mock. Every real backend keeps the `None` default;
    /// the agent actor serves this through `AgentCommand::ReadMockControl`,
    /// so control ownership stays inside the actor tree.
    #[cfg(feature = "test-support")]
    fn mock_control(&self) -> Option<mock::MockControl> {
        None
    }

    /// Request interruption of the currently active turn, if any.
    /// The returned future may resolve after the backend accepts or dispatches
    /// the interrupt request, before the interrupted turn has fully quiesced.
    /// Backends that provide stronger semantics should document them.
    /// Returns false if the backend has terminated or does not support
    /// interruption.
    fn interrupt(&self) -> impl std::future::Future<Output = bool> + Send;

    /// Shut down the live backend session and release any subprocess resources.
    fn shutdown(self) -> impl std::future::Future<Output = ()> + Send
    where
        Self: Sized;
}

pub fn capabilities_for_backend_kind(kind: BackendKind) -> BackendCapabilities {
    match kind {
        BackendKind::Tycode => tycode::TycodeBackend::capabilities(),
        BackendKind::Acp => kiro::KiroBackend::capabilities(),
        BackendKind::Claude => claude::ClaudeBackend::capabilities(),
        BackendKind::Codex => codex::CodexBackend::capabilities(),
        BackendKind::Antigravity => antigravity::AntigravityBackend::capabilities(),
        BackendKind::Hermes => hermes::HermesBackend::capabilities(),
    }
}

#[cfg(feature = "test-support")]
pub fn compaction_capability_is_native(capability: &BackendCompactionCapability) -> bool {
    matches!(
        capability.availability,
        BackendCompactionAvailability::Native { .. }
    )
}

#[cfg(feature = "test-support")]
pub async fn list_sessions_for_backend_kind(
    kind: BackendKind,
) -> Result<Vec<BackendSession>, String> {
    match kind {
        BackendKind::Tycode => tycode::TycodeBackend::list_sessions().await,
        BackendKind::Acp => kiro::KiroBackend::list_sessions().await,
        BackendKind::Claude => claude::ClaudeBackend::list_sessions().await,
        BackendKind::Codex => codex::CodexBackend::list_sessions().await,
        BackendKind::Antigravity => antigravity::AntigravityBackend::list_sessions().await,
        BackendKind::Hermes => hermes::HermesBackend::list_sessions().await,
    }
}

pub(crate) enum PreparedBackendHandle {
    Tycode(Box<tycode::TycodeBackend>),
    Acp(Box<kiro::KiroBackend>),
    Claude(Box<claude::ClaudeBackend>),
    Codex(Box<codex::CodexBackend>),
    Antigravity(Box<antigravity::AntigravityBackend>),
    Hermes(Box<hermes::HermesBackend>),
    Mock { backend: Box<mock::MockBackend> },
}

pub(crate) struct PreparedBackendBinding {
    pub backend: PreparedBackendHandle,
    pub events: EventStream,
    pub provider_session_id: SessionId,
    pub ready: BackendBindingReadyEvidence,
}

impl PreparedBackendHandle {
    pub(crate) async fn shutdown(self) {
        match self {
            Self::Tycode(backend) => Backend::shutdown(*backend).await,
            Self::Acp(backend) => Backend::shutdown(*backend).await,
            Self::Claude(backend) => Backend::shutdown(*backend).await,
            Self::Codex(backend) => Backend::shutdown(*backend).await,
            Self::Antigravity(backend) => Backend::shutdown(*backend).await,
            Self::Hermes(backend) => Backend::shutdown(*backend).await,
            Self::Mock { backend, .. } => Backend::shutdown(*backend).await,
        }
    }
}

pub(crate) async fn prepare_compacted_backend_binding(
    kind: BackendKind,
    spawn: BackendSpawnConfig,
    seed: BackendContextSeed,
) -> Result<PreparedBackendBinding, BackendBindingPrepareError> {
    match kind {
        BackendKind::Tycode => {
            let (backend, events, provider_session_id, ready) =
                prepare_concrete_backend_binding::<tycode::TycodeBackend>(kind, spawn, seed)
                    .await?;
            Ok(PreparedBackendBinding {
                backend: PreparedBackendHandle::Tycode(Box::new(backend)),
                events,
                provider_session_id,
                ready,
            })
        }
        BackendKind::Acp => {
            let (backend, events, provider_session_id, ready) =
                prepare_concrete_backend_binding::<kiro::KiroBackend>(kind, spawn, seed).await?;
            Ok(PreparedBackendBinding {
                backend: PreparedBackendHandle::Acp(Box::new(backend)),
                events,
                provider_session_id,
                ready,
            })
        }
        BackendKind::Claude => {
            let (backend, events, provider_session_id, ready) =
                prepare_concrete_backend_binding::<claude::ClaudeBackend>(kind, spawn, seed)
                    .await?;
            Ok(PreparedBackendBinding {
                backend: PreparedBackendHandle::Claude(Box::new(backend)),
                events,
                provider_session_id,
                ready,
            })
        }
        BackendKind::Codex => {
            let (backend, events, provider_session_id, ready) =
                prepare_concrete_backend_binding::<codex::CodexBackend>(kind, spawn, seed).await?;
            Ok(PreparedBackendBinding {
                backend: PreparedBackendHandle::Codex(Box::new(backend)),
                events,
                provider_session_id,
                ready,
            })
        }
        BackendKind::Antigravity => {
            let (backend, events, provider_session_id, ready) = prepare_concrete_backend_binding::<
                antigravity::AntigravityBackend,
            >(kind, spawn, seed)
            .await?;
            Ok(PreparedBackendBinding {
                backend: PreparedBackendHandle::Antigravity(Box::new(backend)),
                events,
                provider_session_id,
                ready,
            })
        }
        BackendKind::Hermes => {
            let (backend, events, provider_session_id, ready) =
                prepare_concrete_backend_binding::<hermes::HermesBackend>(kind, spawn, seed)
                    .await?;
            Ok(PreparedBackendBinding {
                backend: PreparedBackendHandle::Hermes(Box::new(backend)),
                events,
                provider_session_id,
                ready,
            })
        }
    }
}

pub(crate) async fn prepare_mock_compacted_backend_binding(
    kind: BackendKind,
    spawn: BackendSpawnConfig,
    seed: BackendContextSeed,
) -> Result<PreparedBackendBinding, BackendBindingPrepareError> {
    let (backend, events, provider_session_id, ready) =
        prepare_concrete_backend_binding::<mock::MockBackend>(kind, spawn, seed).await?;
    Ok(PreparedBackendBinding {
        backend: PreparedBackendHandle::Mock {
            backend: Box::new(backend),
        },
        events,
        provider_session_id,
        ready,
    })
}

async fn prepare_concrete_backend_binding<B: Backend>(
    kind: BackendKind,
    spawn: BackendSpawnConfig,
    seed: BackendContextSeed,
) -> Result<(B, EventStream, SessionId, BackendBindingReadyEvidence), BackendBindingPrepareError> {
    let message = seed.render_hidden_bootstrap()?;
    let workspace_roots = seed.workspace_roots;
    let mut bootstrap_spawn = spawn.clone();
    bootstrap_spawn.resolved_spawn_config.tool_policy =
        protocol::ToolPolicy::AllowList { tools: Vec::new() };
    bootstrap_spawn.resolved_spawn_config.access_mode = BackendAccessMode::ReadOnly;
    let initial_input = SendMessagePayload {
        message,
        images: None,
        origin: None,
        tool_response: None,
    };
    let (bootstrap_backend, mut bootstrap_events) =
        B::spawn(workspace_roots.clone(), bootstrap_spawn, initial_input)
            .await
            .map_err(|message| BackendBindingPrepareError::SpawnFailed {
                backend_kind: kind,
                message,
            })?;
    let identity_before = bootstrap_backend.session_id();
    if identity_before.0.trim().is_empty() {
        bootstrap_backend.shutdown().await;
        return Err(BackendBindingPrepareError::ProviderIdentityMissing { backend_kind: kind });
    }
    let mut ready = match drain_prepared_binding_bootstrap(
        kind,
        identity_before.clone(),
        &mut bootstrap_events,
    )
    .await
    {
        Ok(ready) => ready,
        Err(error) => {
            bootstrap_backend.shutdown().await;
            return Err(error);
        }
    };
    let identity_after = bootstrap_backend.session_id();
    if identity_after != identity_before {
        bootstrap_backend.shutdown().await;
        return Err(BackendBindingPrepareError::ProviderIdentityChanged {
            backend_kind: kind,
            before: identity_before,
            after: identity_after,
        });
    }
    bootstrap_backend.shutdown().await;

    let (backend, mut events) = B::resume(workspace_roots, spawn, identity_after.clone())
        .await
        .map_err(|message| BackendBindingPrepareError::ResumeFailed {
            backend_kind: kind,
            provider_session_id: identity_after.clone(),
            message,
        })?;
    if let Some(replay_ready) = events.take_resume_replay_complete() {
        match tokio::time::timeout(std::time::Duration::from_secs(300), replay_ready).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => {
                backend.shutdown().await;
                return Err(BackendBindingPrepareError::BootstrapStreamClosed {
                    backend_kind: kind,
                });
            }
            Err(_) => {
                backend.shutdown().await;
                return Err(BackendBindingPrepareError::BootstrapTimedOut { backend_kind: kind });
            }
        }
    }
    if let Err(error) = drain_prepared_binding_replay(kind, &mut events) {
        backend.shutdown().await;
        return Err(error);
    }
    let resumed_identity = backend.session_id();
    if resumed_identity != identity_after {
        backend.shutdown().await;
        return Err(BackendBindingPrepareError::ProviderIdentityChanged {
            backend_kind: kind,
            before: identity_after,
            after: resumed_identity,
        });
    }
    ready.replay_or_setup_drained = true;
    ready.provider_session_id = resumed_identity.clone();
    Ok((backend, events, resumed_identity, ready))
}

async fn drain_prepared_binding_bootstrap(
    kind: BackendKind,
    provider_session_id: SessionId,
    events: &mut EventStream,
) -> Result<BackendBindingReadyEvidence, BackendBindingPrepareError> {
    let drain = async {
        let mut bootstrap_terminal_seen = false;
        while let Some(event) = events.recv_backend().await {
            match event {
                BackendEvent::Chat(ChatEvent::StreamEnd(_)) => {
                    bootstrap_terminal_seen = true;
                }
                BackendEvent::Chat(ChatEvent::TypingStatusChanged(false))
                    if bootstrap_terminal_seen =>
                {
                    return Ok(BackendBindingReadyEvidence {
                        backend_kind: kind,
                        provider_session_id,
                        bootstrap_terminal_seen: true,
                        provider_idle_seen: true,
                        replay_or_setup_drained: true,
                        unsafe_activity_observed: false,
                    });
                }
                BackendEvent::Chat(ChatEvent::MessageAdded(message))
                    if matches!(
                        message.sender,
                        protocol::MessageSender::Error | protocol::MessageSender::Warning
                    ) =>
                {
                    return Err(BackendBindingPrepareError::BootstrapFailed {
                        backend_kind: kind,
                        message: message.content,
                    });
                }
                BackendEvent::Chat(ChatEvent::OperationCancelled(cancelled)) => {
                    return Err(BackendBindingPrepareError::BootstrapFailed {
                        backend_kind: kind,
                        message: cancelled.message,
                    });
                }
                BackendEvent::Chat(ChatEvent::ToolRequest(request)) => {
                    return Err(BackendBindingPrepareError::BootstrapUnsafeActivity {
                        backend_kind: kind,
                        activity: format!("tool request {}", request.tool_name),
                    });
                }
                BackendEvent::Chat(ChatEvent::ToolExecutionCompleted(completion)) => {
                    return Err(BackendBindingPrepareError::BootstrapUnsafeActivity {
                        backend_kind: kind,
                        activity: format!("tool completion {}", completion.tool_name),
                    });
                }
                BackendEvent::Chat(ChatEvent::Orchestration(_)) => {
                    return Err(BackendBindingPrepareError::BootstrapUnsafeActivity {
                        backend_kind: kind,
                        activity: "orchestration event".to_owned(),
                    });
                }
                BackendEvent::Chat(ChatEvent::ToolProgress(progress)) => {
                    return Err(BackendBindingPrepareError::BootstrapUnsafeActivity {
                        backend_kind: kind,
                        activity: format!("tool progress {:?}", progress.update),
                    });
                }
                BackendEvent::Chat(ChatEvent::TaskUpdate(_)) => {
                    return Err(BackendBindingPrepareError::BootstrapUnsafeActivity {
                        backend_kind: kind,
                        activity: "provider task state".to_owned(),
                    });
                }
                BackendEvent::Chat(_)
                | BackendEvent::ModelRequestTokenUsage(_)
                | BackendEvent::Compaction(_) => {}
            }
        }
        Err(BackendBindingPrepareError::BootstrapStreamClosed { backend_kind: kind })
    };
    tokio::time::timeout(std::time::Duration::from_secs(300), drain)
        .await
        .map_err(|_| BackendBindingPrepareError::BootstrapTimedOut { backend_kind: kind })?
}

fn drain_prepared_binding_replay(
    kind: BackendKind,
    events: &mut EventStream,
) -> Result<(), BackendBindingPrepareError> {
    loop {
        match events.try_recv_backend() {
            Ok(BackendEvent::Chat(ChatEvent::MessageAdded(message)))
                if matches!(
                    message.sender,
                    protocol::MessageSender::Error | protocol::MessageSender::Warning
                ) =>
            {
                return Err(BackendBindingPrepareError::BootstrapFailed {
                    backend_kind: kind,
                    message: message.content,
                });
            }
            Ok(BackendEvent::Chat(ChatEvent::OperationCancelled(cancelled))) => {
                return Err(BackendBindingPrepareError::BootstrapFailed {
                    backend_kind: kind,
                    message: cancelled.message,
                });
            }
            Ok(BackendEvent::Chat(ChatEvent::ToolRequest(request))) => {
                return Err(BackendBindingPrepareError::BootstrapUnsafeActivity {
                    backend_kind: kind,
                    activity: format!("replayed tool request {}", request.tool_name),
                });
            }
            Ok(BackendEvent::Chat(ChatEvent::ToolExecutionCompleted(completion))) => {
                return Err(BackendBindingPrepareError::BootstrapUnsafeActivity {
                    backend_kind: kind,
                    activity: format!("replayed tool completion {}", completion.tool_name),
                });
            }
            Ok(BackendEvent::Chat(ChatEvent::ToolProgress(progress))) => {
                return Err(BackendBindingPrepareError::BootstrapUnsafeActivity {
                    backend_kind: kind,
                    activity: format!("replayed tool progress {:?}", progress.update),
                });
            }
            Ok(BackendEvent::Chat(ChatEvent::TaskUpdate(_)))
            | Ok(BackendEvent::Chat(ChatEvent::Orchestration(_))) => {
                return Err(BackendBindingPrepareError::BootstrapUnsafeActivity {
                    backend_kind: kind,
                    activity: "replayed provider task or orchestration state".to_owned(),
                });
            }
            Ok(BackendEvent::Chat(_))
            | Ok(BackendEvent::ModelRequestTokenUsage(_))
            | Ok(BackendEvent::Compaction(_)) => {}
            Err(mpsc::error::TryRecvError::Empty) => return Ok(()),
            Err(mpsc::error::TryRecvError::Disconnected) => {
                return Err(BackendBindingPrepareError::BootstrapStreamClosed {
                    backend_kind: kind,
                });
            }
        }
    }
}

pub(crate) fn backend_fork_unsupported_message(backend_kind: BackendKind) -> String {
    format!("{backend_kind:?} backend does not support session fork")
}

pub(crate) fn empty_session_settings_schema(backend_kind: BackendKind) -> SessionSettingsSchema {
    SessionSettingsSchema {
        backend_kind,
        fields: Vec::new(),
    }
}

pub(crate) fn session_settings_schema_for_backend(
    backend_kind: BackendKind,
) -> SessionSettingsSchema {
    match backend_kind {
        BackendKind::Tycode => tycode::TycodeBackend::session_settings_schema(),
        BackendKind::Acp => kiro::KiroBackend::session_settings_schema(),
        BackendKind::Claude => claude::ClaudeBackend::session_settings_schema(),
        BackendKind::Codex => codex::CodexBackend::session_settings_schema(),
        BackendKind::Antigravity => antigravity::AntigravityBackend::session_settings_schema(),
        BackendKind::Hermes => hermes::HermesBackend::session_settings_schema(),
    }
}

/// The host-level deep-configuration schema for a backend, if it exposes one.
pub(crate) fn backend_config_schema_for_backend(
    backend_kind: BackendKind,
) -> Option<BackendConfigSchema> {
    match backend_kind {
        BackendKind::Tycode => tycode::TycodeBackend::backend_config_schema(),
        BackendKind::Acp => kiro::KiroBackend::backend_config_schema(),
        BackendKind::Claude => claude::ClaudeBackend::backend_config_schema(),
        BackendKind::Codex => codex::CodexBackend::backend_config_schema(),
        BackendKind::Antigravity => antigravity::AntigravityBackend::backend_config_schema(),
        BackendKind::Hermes => hermes::HermesBackend::backend_config_schema(),
    }
}

/// Host/build-level backend config schemas. This is a catalog of configurable
/// backends, not a projection of the current enabled-backend list.
pub(crate) fn backend_config_schema_catalog() -> Vec<BackendConfigSchema> {
    [
        BackendKind::Tycode,
        BackendKind::Acp,
        BackendKind::Claude,
        BackendKind::Codex,
        BackendKind::Antigravity,
        BackendKind::Hermes,
    ]
    .into_iter()
    .filter_map(backend_config_schema_for_backend)
    .collect()
}

/// Drop keys/values that the backend's config schema does not accept. A backend
/// with no config schema sanitizes to empty (it stores no deep config).
pub(crate) fn sanitize_backend_config_values(
    backend_kind: BackendKind,
    values: &BackendConfigValues,
) -> BackendConfigValues {
    sanitize_backend_config_values_with_schema(
        backend_config_schema_for_backend(backend_kind).as_ref(),
        values,
    )
}

fn sanitize_backend_config_values_with_schema(
    schema: Option<&BackendConfigSchema>,
    values: &BackendConfigValues,
) -> BackendConfigValues {
    let Some(schema) = schema else {
        return BackendConfigValues::default();
    };
    let mut sanitized = BackendConfigValues::default();
    for (key, value) in &values.0 {
        let Some(field) = schema.fields.iter().find(|field| field.key == *key) else {
            continue;
        };
        if validate_backend_config_value(key, value, &field.field_type).is_ok() {
            sanitized.0.insert(key.clone(), value.clone());
        }
    }
    sanitized
}

pub(crate) fn validate_backend_config_values(
    backend_kind: BackendKind,
    values: &BackendConfigValues,
) -> Result<BackendConfigValues, String> {
    validate_backend_config_values_with_schema(
        backend_kind,
        backend_config_schema_for_backend(backend_kind).as_ref(),
        values,
    )
}

fn validate_backend_config_values_with_schema(
    backend_kind: BackendKind,
    schema: Option<&BackendConfigSchema>,
    values: &BackendConfigValues,
) -> Result<BackendConfigValues, String> {
    if values.0.is_empty() {
        return Ok(BackendConfigValues::default());
    }
    let Some(schema) = schema else {
        return Err(format!(
            "{backend_kind:?} does not support backend configuration"
        ));
    };
    let mut sanitized = BackendConfigValues::default();
    for (key, value) in &values.0 {
        let Some(field) = schema.fields.iter().find(|field| field.key == *key) else {
            return Err(format!(
                "{backend_kind:?} backend config key '{key}' is not defined by its schema"
            ));
        };
        validate_backend_config_value(key, value, &field.field_type)
            .map_err(|err| format!("{backend_kind:?} backend config invalid: {err}"))?;
        sanitized.0.insert(key.clone(), value.clone());
    }
    Ok(sanitized)
}

fn validate_backend_config_value(
    key: &str,
    value: &SessionSettingValue,
    field_type: &BackendConfigFieldType,
) -> Result<(), String> {
    if matches!(value, SessionSettingValue::Null) {
        if let BackendConfigFieldType::Select {
            nullable: false, ..
        } = field_type
        {
            return Err(format!("backend config '{}' does not accept null", key));
        }
        return Ok(());
    }
    match (value, field_type) {
        (SessionSettingValue::String(_), BackendConfigFieldType::Text { .. }) => Ok(()),
        (SessionSettingValue::String(_), BackendConfigFieldType::Secret { .. }) => Ok(()),
        (SessionSettingValue::String(actual), BackendConfigFieldType::Select { options, .. }) => {
            if options.iter().any(|option| option.value == *actual) {
                Ok(())
            } else {
                Err(format!(
                    "invalid backend config '{}' value '{}'",
                    key, actual
                ))
            }
        }
        (SessionSettingValue::Bool(_), BackendConfigFieldType::Toggle { .. }) => Ok(()),
        (
            SessionSettingValue::Integer(actual),
            BackendConfigFieldType::Integer { min, max, .. },
        ) if (*min..=*max).contains(actual) => Ok(()),
        _ => Err(format!("invalid backend config '{}' type", key)),
    }
}

pub(crate) fn resolve_backend_session_settings(
    backend_kind: BackendKind,
    config: &BackendSpawnConfig,
) -> SessionSettingsValues {
    match backend_kind {
        BackendKind::Tycode => tycode::resolve_session_settings(config),
        BackendKind::Acp => kiro::resolve_session_settings(config),
        BackendKind::Claude => claude::resolve_session_settings(config),
        BackendKind::Codex => codex::resolve_session_settings(config),
        BackendKind::Antigravity => antigravity::resolve_session_settings(config),
        BackendKind::Hermes => hermes::resolve_session_settings(config),
    }
}

pub(crate) fn validate_session_settings_values(
    schema: &SessionSettingsSchema,
    values: &SessionSettingsValues,
) -> Result<(), String> {
    let fields_by_key = schema
        .fields
        .iter()
        .map(|field| (field.key.as_str(), field))
        .collect::<HashMap<_, _>>();

    for (key, value) in &values.0 {
        if schema.backend_kind == BackendKind::Tycode
            && key == "tyde_conformance_model"
            && matches!(value, SessionSettingValue::String(_))
        {
            continue;
        }
        let Some(field) = fields_by_key.get(key.as_str()) else {
            return Err(format!(
                "unknown session setting '{}' for backend {:?}",
                key, schema.backend_kind
            ));
        };
        validate_session_setting_value(key, value, field, values)?;
    }

    Ok(())
}

pub(crate) fn validate_runtime_session_settings_update(
    backend_kind: BackendKind,
    current: &SessionSettingsValues,
    update: &SessionSettingsValues,
) -> Result<(), String> {
    match backend_kind {
        BackendKind::Tycode => tycode::validate_runtime_session_settings_update(update),
        BackendKind::Hermes => hermes::validate_runtime_session_settings_update(current, update),
        BackendKind::Acp | BackendKind::Claude | BackendKind::Codex | BackendKind::Antigravity => {
            Ok(())
        }
    }
}

pub(crate) fn sanitize_session_settings_values(
    schema: &SessionSettingsSchema,
    values: &SessionSettingsValues,
) -> SessionSettingsValues {
    let mut sanitized = SessionSettingsValues::default();
    for (key, value) in &values.0 {
        let Some(field) = schema.fields.iter().find(|field| field.key == *key) else {
            continue;
        };
        if validate_session_setting_value(key, value, field, values).is_ok() {
            sanitized.0.insert(key.clone(), value.clone());
        }
    }
    sanitized
}

pub(crate) fn apply_session_settings_update(
    values: &mut SessionSettingsValues,
    update: &SessionSettingsValues,
) {
    for (key, value) in &update.0 {
        match value {
            SessionSettingValue::Null => {
                values.0.remove(key);
            }
            _ => {
                values.0.insert(key.clone(), value.clone());
            }
        }
    }
}

/// Static Low/High tier mappings. Dynamic Codex tiers are resolved from its
/// live session schema by the host.
pub(crate) fn builtin_tier_config(kind: BackendKind) -> BackendTierConfig {
    let defaults: fn(SpawnCostHint) -> SessionSettingsValues = match kind {
        BackendKind::Claude => claude::claude_cost_hint_defaults,
        BackendKind::Codex | BackendKind::Tycode | BackendKind::Hermes => {
            |_| SessionSettingsValues::default()
        }
        BackendKind::Antigravity => antigravity::antigravity_cost_hint_defaults,
        BackendKind::Acp => kiro::kiro_cost_hint_defaults,
    };
    BackendTierConfig {
        low: defaults(SpawnCostHint::Low),
        high: defaults(SpawnCostHint::High),
    }
}

pub(crate) fn resolve_settings<F>(
    config: &BackendSpawnConfig,
    schema: &SessionSettingsSchema,
    cost_hint_defaults: F,
) -> SessionSettingsValues
where
    F: Fn(SpawnCostHint) -> SessionSettingsValues,
{
    let mut resolved = SessionSettingsValues::default();

    if let Some(hint) = config.cost_hint {
        resolved.0.extend(cost_hint_defaults(hint).0);
    }

    if let Some(session_settings) = config.session_settings.as_ref() {
        apply_session_settings_update(&mut resolved, session_settings);
    }

    for field in &schema.fields {
        if !resolved.0.contains_key(&field.key)
            && let Some(default) = schema_field_default(&field.field_type)
        {
            resolved.0.insert(field.key.clone(), default);
        }
    }

    resolved
}

pub(crate) fn session_settings_to_json(values: &SessionSettingsValues) -> Value {
    let mut object = serde_json::Map::with_capacity(values.0.len());
    for (key, value) in &values.0 {
        object.insert(key.clone(), session_setting_value_to_json(value));
    }
    Value::Object(object)
}

pub(crate) fn render_combined_spawn_instructions(config: &ResolvedSpawnConfig) -> Option<String> {
    let mut sections = Vec::new();
    if config.access_mode == BackendAccessMode::ReadOnly {
        sections.push(READ_ONLY_ACCESS_MODE_INSTRUCTIONS.to_string());
    }
    if let Some(instructions) = config
        .instructions
        .as_ref()
        .map(|text| text.trim())
        .filter(|text| !text.is_empty())
    {
        sections.push(format!("Custom agent instructions:\n{instructions}"));
    }
    if !config.steering_body.trim().is_empty() {
        sections.push(format!("Steering:\n{}", config.steering_body.trim()));
    }
    // Only backends with no discovery seam get bodies here; the rest are
    // pointed at each skill's directory, or name it, from their own adapter.
    if config.skill_delivery.renders_inline_bodies() {
        let skill_blocks = config
            .skills
            .iter()
            .filter_map(|skill| {
                skill
                    .inline_body()
                    .map(|body| format!("Skill: {}\n{}", skill.name, body.trim()))
            })
            .collect::<Vec<_>>();
        if !skill_blocks.is_empty() {
            sections.push(format!("Skills:\n{}", skill_blocks.join("\n\n")));
        }
    }

    if sections.is_empty() {
        None
    } else {
        Some(sections.join("\n\n"))
    }
}

pub(crate) fn schema_field_default(
    field_type: &SessionSettingFieldType,
) -> Option<SessionSettingValue> {
    match field_type {
        SessionSettingFieldType::Select {
            default: Some(default),
            ..
        } => Some(SessionSettingValue::String(default.clone())),
        SessionSettingFieldType::Select { default: None, .. } => None,
        SessionSettingFieldType::Toggle { default } => Some(SessionSettingValue::Bool(*default)),
        SessionSettingFieldType::Integer { default, .. } => {
            Some(SessionSettingValue::Integer(*default))
        }
    }
}

fn validate_session_setting_value(
    key: &str,
    value: &SessionSettingValue,
    field: &SessionSettingField,
    values: &SessionSettingsValues,
) -> Result<(), String> {
    let field_type = &field.field_type;
    if matches!(value, SessionSettingValue::Null) {
        if let SessionSettingFieldType::Select {
            nullable: false, ..
        } = field_type
        {
            return Err(format!("session setting '{}' does not accept null", key));
        }
        return Ok(());
    }

    match (value, field_type) {
        (SessionSettingValue::String(actual), SessionSettingFieldType::Select { .. }) => {
            if field
                .select_options(values)
                .is_some_and(|options| options.iter().any(|option| option.value == *actual))
            {
                Ok(())
            } else {
                Err(format!(
                    "invalid session setting '{}' value '{}'",
                    key, actual
                ))
            }
        }
        (SessionSettingValue::Bool(_), SessionSettingFieldType::Toggle { .. }) => Ok(()),
        (
            SessionSettingValue::Integer(actual),
            SessionSettingFieldType::Integer { min, max, .. },
        ) if (*min..=*max).contains(actual) => Ok(()),
        _ => Err(format!("invalid session setting '{}' type", key)),
    }
}

fn session_setting_value_to_json(value: &SessionSettingValue) -> Value {
    match value {
        SessionSettingValue::String(value) => Value::String(value.clone()),
        SessionSettingValue::Bool(value) => Value::Bool(*value),
        SessionSettingValue::Integer(value) => Value::Number((*value).into()),
        SessionSettingValue::Null => Value::Null,
    }
}
