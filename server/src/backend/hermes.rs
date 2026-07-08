use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{fs, io};

use command_group::{AsyncCommandGroup, AsyncGroupChild};
use protocol::{
    AgentInput, BackendConfigField, BackendConfigFieldType, BackendConfigPersistenceMode,
    BackendConfigSchema, BackendConfigValues, BackendKind, BackendSetupDiagnosticCode, ChatEvent,
    ChatMessage, MessageSender, MessageTokenUsage, ModelInfo, OperationCancelledData, SelectOption,
    SendMessageToolResponse, SessionId, SessionSettingField, SessionSettingFieldType,
    SessionSettingValue, SessionSettingsSchema, SessionSettingsValues, StreamEndData,
    StreamStartData, StreamTextDeltaData, TokenUsage, TokenUsageUnavailableReason,
    ToolExecutionCompletedData, ToolExecutionResult, ToolProgressData, ToolProgressUpdate,
    ToolRequest, ToolRequestType, ToolUseData,
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStderr, ChildStdout, Command};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use crate::agent::customization::ResolvedSpawnConfig;
use crate::backend::{
    Backend, BackendSession, BackendSpawnConfig, BackendStartupError, EventStream,
    backend_config_text, backend_fork_unsupported_message, render_combined_spawn_instructions,
    resolve_settings as resolve_backend_settings, tyde_owned_no_root_cwd,
};
use crate::process_env;

const HERMES_AGENT_NAME: &str = "hermes";
const HERMES_PYTHON_MODULE: &str = "tui_gateway.entry";
const HERMES_EXECUTABLE_ENV: &str = "HERMES_EXECUTABLE";
const HERMES_CLI_BINARY: &str = "hermes";
const HERMES_STARTUP_TIMEOUT_ENV: &str = "HERMES_TUI_STARTUP_TIMEOUT_MS";
const HERMES_RPC_TIMEOUT_ENV: &str = "HERMES_TUI_RPC_TIMEOUT_MS";
const HERMES_REMOTE_PYTHON_ENV: &str = "TYDE_REMOTE_HERMES_PYTHON";
const HERMES_STARTUP_TIMEOUT: Duration = Duration::from_secs(15);
const HERMES_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const HERMES_USAGE_TIMEOUT: Duration = Duration::from_secs(2);
const HERMES_MODEL_PROVIDER_FLAG: &str = " --provider ";

#[cfg(test)]
static TEST_HERMES_PYTHON: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
#[cfg(test)]
static TEST_HERMES_EXECUTABLE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
#[cfg(test)]
pub(crate) static TEST_HERMES_OVERRIDE_LOCK: tokio::sync::Mutex<()> =
    tokio::sync::Mutex::const_new(());

#[derive(Clone)]
pub struct HermesBackend {
    command_tx: mpsc::UnboundedSender<HermesBackendCommand>,
    session_id: SessionId,
}

enum HermesBackendCommand {
    Input(AgentInput),
    Interrupt(oneshot::Sender<bool>),
    Shutdown,
}

#[derive(Clone)]
struct HermesGatewayHandle {
    tx: mpsc::UnboundedSender<HermesGatewayCommand>,
    request_timeout: Duration,
}

enum HermesGatewayCommand {
    Request {
        method: String,
        params: Value,
        reply: oneshot::Sender<Result<Value, String>>,
    },
    Shutdown,
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
    cwd: Option<String>,
    remote_host: Option<String>,
    display_program: String,
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

struct HermesSessionIds {
    live_session_id: String,
    stored_session_id: SessionId,
}

struct HermesSessionActor {
    gateway: HermesGatewayHandle,
    live_session_id: String,
    mapper: HermesEventMapper,
    events_tx: mpsc::UnboundedSender<ChatEvent>,
    command_rx: mpsc::UnboundedReceiver<HermesBackendCommand>,
    gateway_events_rx: mpsc::UnboundedReceiver<HermesGatewayEvent>,
}

#[derive(Default)]
struct HermesEventMapper {
    current_message_id: Option<String>,
    current_text: String,
    current_reasoning_seen: bool,
    model: Option<String>,
    provider: Option<String>,
    pending_tools: HashMap<String, String>,
    turn_tools: HashMap<String, String>,
    pending_approval_tool_id: Option<String>,
    last_cumulative_usage: Option<TokenUsage>,
    awaiting_interrupted_complete: bool,
    session_info_emitted: bool,
    approval_counter: u64,
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

fn hermes_backend_config_schema() -> BackendConfigSchema {
    BackendConfigSchema {
        backend_kind: BackendKind::Hermes,
        persistence_mode: BackendConfigPersistenceMode::TydeSettingsStore,
        fields: vec![
            BackendConfigField {
                key: "default_model".to_string(),
                label: "Default Model".to_string(),
                description: Some(
                    "Model id every new Hermes session starts with (e.g. \
                     anthropic/claude-sonnet-5). The per-session Model setting \
                     overrides this. Passed to Hermes verbatim, so it also works \
                     for remote workspaces where a locally probed list would be wrong."
                        .to_string(),
                ),
                field_type: BackendConfigFieldType::Text {
                    default: None,
                    placeholder: Some("provider/model-id".to_string()),
                    multiline: false,
                },
            },
            BackendConfigField {
                key: "default_provider".to_string(),
                label: "Default Provider".to_string(),
                description: Some(
                    "Provider slug for the default model (e.g. openrouter, anthropic). \
                     Leave blank to let Hermes choose."
                        .to_string(),
                ),
                field_type: BackendConfigFieldType::Text {
                    default: None,
                    placeholder: Some("openrouter".to_string()),
                    multiline: false,
                },
            },
            BackendConfigField {
                key: "api_base_url".to_string(),
                label: "API Base URL".to_string(),
                description: Some(
                    "Optional base URL override passed to Hermes at session start. \
                     Leave blank to use Hermes defaults."
                        .to_string(),
                ),
                field_type: BackendConfigFieldType::Text {
                    default: None,
                    placeholder: Some("https://…".to_string()),
                    multiline: false,
                },
            },
        ],
    }
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
            field_type: SessionSettingFieldType::Toggle { default: false },
        },
    ]
}

impl Backend for HermesBackend {
    fn session_settings_schema() -> SessionSettingsSchema {
        SessionSettingsSchema {
            backend_kind: BackendKind::Hermes,
            fields: hermes_base_session_fields(),
        }
    }

    fn backend_config_schema() -> Option<BackendConfigSchema> {
        Some(hermes_backend_config_schema())
    }

    async fn spawn(
        workspace_roots: Vec<String>,
        config: BackendSpawnConfig,
        initial_input: protocol::SendMessagePayload,
    ) -> Result<(Self, EventStream), String> {
        reject_unverified_capabilities(&config, &initial_input)?;
        let resolved_settings = resolve_session_settings(&config);
        let (gateway, gateway_events_rx) = HermesGatewayHandle::spawn(&workspace_roots).await?;
        let create_params = build_session_create_params(
            &workspace_roots,
            &config.resolved_spawn_config,
            &resolved_settings,
            &config.backend_config,
        )?;
        let create = gateway.request("session.create", create_params).await?;
        let ids = parse_session_create_ids(&create)?;
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let actor = HermesSessionActor {
            gateway: gateway.clone(),
            live_session_id: ids.live_session_id.clone(),
            mapper: HermesEventMapper::default(),
            events_tx,
            command_rx,
            gateway_events_rx,
        };
        tokio::spawn(actor.run(Some(initial_input), None));

        Ok((
            Self {
                command_tx,
                session_id: ids.stored_session_id,
            },
            EventStream::new(events_rx),
        ))
    }

    async fn resume(
        workspace_roots: Vec<String>,
        config: BackendSpawnConfig,
        session_id: SessionId,
    ) -> Result<(Self, EventStream), String> {
        reject_unverified_resume_capabilities(&config)?;
        let (gateway, gateway_events_rx) = HermesGatewayHandle::spawn(&workspace_roots).await?;
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
        let (resume_replay_complete_tx, resume_replay_complete_rx) = oneshot::channel();
        let stored_session_id = SessionId(resumed);
        let actor = HermesSessionActor {
            gateway: gateway.clone(),
            live_session_id,
            mapper: HermesEventMapper::default(),
            events_tx,
            command_rx,
            gateway_events_rx,
        };
        tokio::spawn(actor.run(None, Some((replay_events, resume_replay_complete_tx))));

        Ok((
            Self {
                command_tx,
                session_id: stored_session_id,
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
            backend_fork_unsupported_message(BackendKind::Hermes),
        ))
    }

    async fn list_sessions() -> Result<Vec<BackendSession>, String> {
        let (gateway, _gateway_events_rx) = HermesGatewayHandle::spawn(&[]).await?;
        let result = gateway
            .request("session.list", json!({ "limit": 200 }))
            .await;
        gateway.shutdown().await;
        parse_session_list(&result?)
    }

    fn session_id(&self) -> SessionId {
        self.session_id.clone()
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
        let _ = self.command_tx.send(HermesBackendCommand::Shutdown);
    }
}

pub(crate) async fn probe_session_settings_schema(
    workspace_roots: &[String],
) -> Result<SessionSettingsSchema, String> {
    let (gateway, _events) = HermesGatewayHandle::spawn(workspace_roots).await?;
    let options = gateway.request("model.options", json!({})).await;
    gateway.shutdown().await;
    session_settings_schema_from_model_options(&options?)
}

pub(crate) async fn probe_backend_config_snapshot(
    workspace_roots: &[String],
) -> Result<BackendConfigValues, String> {
    let (gateway, _events) = HermesGatewayHandle::spawn(workspace_roots).await?;
    let options = gateway.request("model.options", json!({})).await;
    gateway.shutdown().await;
    hermes_backend_config_snapshot_from_model_options(&options?)
}

fn hermes_backend_config_snapshot_from_model_options(
    value: &Value,
) -> Result<BackendConfigValues, String> {
    let mut values = BackendConfigValues::default();
    if let Some(model) = optional_present_non_empty_string(value, &["model"], "model.options")? {
        values.0.insert(
            "default_model".to_string(),
            SessionSettingValue::String(model),
        );
    }
    if let Some(provider) =
        optional_present_non_empty_string(value, &["provider"], "model.options")?
    {
        values.0.insert(
            "default_provider".to_string(),
            SessionSettingValue::String(provider),
        );
    }
    Ok(values)
}

impl HermesSessionActor {
    async fn run(
        mut self,
        initial_input: Option<protocol::SendMessagePayload>,
        replay: Option<(Vec<ChatEvent>, oneshot::Sender<()>)>,
    ) {
        if let Some((events, barrier)) = replay {
            for event in events {
                if self.events_tx.send(event).is_err() {
                    let _ = barrier.send(());
                    self.gateway.shutdown().await;
                    return;
                }
            }
            let _ = barrier.send(());
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
                        HermesBackendCommand::Interrupt(reply) => {
                            let ok = self.handle_interrupt().await;
                            let _ = reply.send(ok);
                        }
                        HermesBackendCommand::Shutdown => break,
                    }
                }
            }
        }

        self.gateway.shutdown().await;
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
                self.handle_settings_update(payload.values).await;
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
                        self.emit(ChatEvent::ToolExecutionCompleted(
                            ToolExecutionCompletedData {
                                tool_call_id,
                                tool_name: "approval.request".to_string(),
                                tool_result: ToolExecutionResult::Other { result },
                                success: true,
                                error: None,
                            },
                        ));
                        self.emit(ChatEvent::TypingStatusChanged(true));
                    }
                    Err(err) => self.emit_error(format!("Hermes approval.respond failed: {err}")),
                }
            }
        }
    }

    async fn handle_settings_update(&mut self, values: SessionSettingsValues) {
        for (key, value) in values.0 {
            match (key.as_str(), value) {
                ("model", SessionSettingValue::String(model)) if !model.trim().is_empty() => {
                    let Some(selection) = parse_hermes_model_setting(&model) else {
                        self.emit_error(format!("invalid Hermes model setting '{model}'"));
                        continue;
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
                        Err(err) => {
                            self.emit_error(format!("Hermes config.set model failed: {err}"))
                        }
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
                        self.emit_error(format!("Hermes config.set reasoning failed: {err}"));
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
                        self.emit_error(format!("Hermes config.set fast failed: {err}"));
                    }
                }
                (unknown, _) => {
                    self.emit_error(format!("unsupported Hermes session setting '{unknown}'"))
                }
            }
        }
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
                if event_type == "message.complete" {
                    payload = self.enrich_message_complete_payload(payload).await;
                }
                let mapped = self.mapper.map_event(&event_type, payload);
                for event in mapped {
                    self.emit(event);
                }
                true
            }
            HermesGatewayEvent::ProtocolError(message) => {
                self.emit_turn_failure(format!("Hermes gateway protocol error: {message}"));
                true
            }
            HermesGatewayEvent::Stderr(line) => {
                self.emit(ChatEvent::MessageAdded(warning_message(format!(
                    "Hermes stderr: {line}"
                ))));
                true
            }
            HermesGatewayEvent::Closed(exit_code) => {
                self.emit_turn_failure(match exit_code {
                    Some(code) => format!("Hermes gateway exited with code {code}"),
                    None => "Hermes gateway exited".to_string(),
                });
                false
            }
        }
    }

    async fn enrich_message_complete_payload(&mut self, payload: Option<Value>) -> Option<Value> {
        let has_turn_usage = payload
            .as_ref()
            .and_then(|payload| payload.get("usage"))
            .and_then(token_usage_from_value)
            .is_some();
        let usage_result = tokio::time::timeout(
            HERMES_USAGE_TIMEOUT,
            self.gateway.request(
                "session.usage",
                json!({ "session_id": self.live_session_id }),
            ),
        )
        .await;
        let usage = match usage_result {
            Ok(Ok(value)) => value,
            Ok(Err(err)) => {
                self.emit(ChatEvent::MessageAdded(warning_message(format!(
                    "Hermes session.usage failed: {err}"
                ))));
                return payload;
            }
            Err(_) => {
                self.emit(ChatEvent::MessageAdded(warning_message(
                    "Hermes session.usage timed out",
                )));
                return payload;
            }
        };

        let Some(cumulative) = token_usage_from_value(&usage) else {
            self.emit(ChatEvent::MessageAdded(warning_message(
                "Hermes session.usage did not report token counts",
            )));
            return payload;
        };
        let turn_usage = token_usage_delta(self.mapper.last_cumulative_usage.as_ref(), &cumulative);
        self.mapper.last_cumulative_usage = Some(cumulative.clone());

        match payload {
            Some(mut payload) => {
                if let Some(object) = payload.as_object_mut() {
                    if !has_turn_usage {
                        object.insert(
                            "usage".to_string(),
                            token_usage_to_gateway_value(&turn_usage),
                        );
                    }
                    object.insert(
                        "cumulative_usage".to_string(),
                        token_usage_to_gateway_value(&cumulative),
                    );
                }
                Some(payload)
            }
            None => None,
        }
    }

    fn emit(&self, event: ChatEvent) {
        let _ = self.events_tx.send(event);
    }

    fn emit_error(&self, message: impl Into<String>) {
        let _ = self
            .events_tx
            .send(ChatEvent::MessageAdded(error_message(message.into())));
    }

    fn emit_turn_failure(&mut self, message: impl Into<String>) {
        for event in self.mapper.fail_active_turn(message.into()) {
            self.emit(event);
        }
    }
}

impl HermesGatewayHandle {
    async fn spawn(
        workspace_roots: &[String],
    ) -> Result<(Self, mpsc::UnboundedReceiver<HermesGatewayEvent>), String> {
        let target = resolve_gateway_spawn_target(workspace_roots).await?;
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
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (ready_tx, ready_rx) = oneshot::channel();

        spawn_stdout_reader(stdout, inbound_tx.clone());
        spawn_stderr_reader(stderr, inbound_tx.clone());
        spawn_child_waiter(child, inbound_tx);
        tokio::spawn(run_gateway_actor(
            stdin,
            command_rx,
            inbound_rx,
            event_tx,
            Some(ready_tx),
        ));

        let handle = Self {
            tx: command_tx,
            request_timeout,
        };

        match tokio::time::timeout(startup_timeout, ready_rx).await {
            Ok(Ok(Ok(()))) => Ok((handle, event_rx)),
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
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(HermesGatewayCommand::Request {
                method: method.to_string(),
                params,
                reply: reply_tx,
            })
            .map_err(|_| format!("Hermes gateway is closed; cannot send {method}"))?;
        match tokio::time::timeout(self.request_timeout, reply_rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(format!("Hermes gateway closed while waiting for {method}")),
            Err(_) => Err(format!("Hermes request timed out for method '{method}'")),
        }
    }

    async fn shutdown(&self) {
        let _ = self.tx.send(HermesGatewayCommand::Shutdown);
    }
}

async fn run_gateway_actor(
    mut stdin: tokio::process::ChildStdin,
    mut command_rx: mpsc::UnboundedReceiver<HermesGatewayCommand>,
    mut inbound_rx: mpsc::UnboundedReceiver<HermesGatewayInbound>,
    event_tx: mpsc::UnboundedSender<HermesGatewayEvent>,
    mut ready_tx: Option<oneshot::Sender<Result<(), String>>>,
) {
    let mut next_id = 1_u64;
    let mut pending: HashMap<u64, oneshot::Sender<Result<Value, String>>> = HashMap::new();

    loop {
        tokio::select! {
            maybe_command = command_rx.recv() => {
                let Some(command) = maybe_command else { break; };
                match command {
                    HermesGatewayCommand::Request { method, params, reply } => {
                        let id = next_id;
                        next_id = next_id.saturating_add(1);
                        let frame = json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "method": method,
                            "params": params,
                        });
                        let line = format!("{}\n", frame);
                        match stdin.write_all(line.as_bytes()).await {
                            Ok(()) => match stdin.flush().await {
                                Ok(()) => {
                                    pending.insert(id, reply);
                                }
                                Err(err) => {
                                    let _ = reply.send(Err(format!("Failed to flush Hermes request {id}: {err}")));
                                }
                            },
                            Err(err) => {
                                let _ = reply.send(Err(format!("Failed to write Hermes request {id}: {err}")));
                            }
                        }
                    }
                    HermesGatewayCommand::Shutdown => break,
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
                        let _ = event_tx.send(HermesGatewayEvent::Stderr(line));
                    }
                    HermesGatewayInbound::Closed(code) => {
                        for (_id, reply) in pending.drain() {
                            let _ = reply.send(Err(match code {
                                Some(code) => format!("Hermes gateway exited with code {code}"),
                                None => "Hermes gateway exited".to_string(),
                            }));
                        }
                        if let Some(tx) = ready_tx.take() {
                            let _ = tx.send(Err(match code {
                                Some(code) => format!("Hermes gateway exited with code {code} before gateway.ready"),
                                None => "Hermes gateway exited before gateway.ready".to_string(),
                            }));
                        }
                        let _ = event_tx.send(HermesGatewayEvent::Closed(code));
                        break;
                    }
                }
            }
        }
    }

    for (_id, reply) in pending.drain() {
        let _ = reply.send(Err("Hermes gateway actor stopped".to_string()));
    }
}

fn handle_gateway_stdout_line(
    line: &str,
    pending: &mut HashMap<u64, oneshot::Sender<Result<Value, String>>>,
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
            let _ = reply.send(Err(match code {
                Some(code) => format!("Hermes JSON-RPC error {code}: {message}"),
                None => format!("Hermes JSON-RPC error: {message}"),
            }));
        } else if let Some(result) = value.get("result") {
            let _ = reply.send(Ok(result.clone()));
        } else {
            let _ = reply.send(Err(format!(
                "Hermes response {id} missing both result and error"
            )));
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
) {
    tokio::spawn(async move {
        let code = match child.wait().await {
            Ok(status) => status.code(),
            Err(_) => None,
        };
        let _ = inbound_tx.send(HermesGatewayInbound::Closed(code));
    });
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
    fn map_event(&mut self, event_type: &str, payload: Option<Value>) -> Vec<ChatEvent> {
        let result = match event_type {
            "gateway.ready" => Ok(Vec::new()),
            "session.info" => self.map_session_info(payload),
            "status.update" => self.map_status_update(payload),
            "message.start" => self.map_message_start(),
            "message.delta" => self.map_message_delta(payload),
            "message.complete" => self.map_message_complete(payload),
            "thinking.delta" | "reasoning.delta" => self.map_reasoning_delta(event_type, payload),
            "reasoning.available" => self.map_reasoning_available(payload),
            "tool.start" => self.map_tool_start(payload),
            "tool.progress" => self.map_tool_progress(payload),
            "tool.complete" => self.map_tool_complete(payload),
            "approval.request" => self.map_approval_request(payload),
            "error" => self.map_error(payload),
            event if event.starts_with("subagent.") => {
                Ok(vec![ChatEvent::MessageAdded(warning_message(format!(
                    "Hermes delegation event '{event}' is not mapped to Tyde SubAgentProgress yet"
                )))])
            }
            other => Ok(vec![ChatEvent::MessageAdded(warning_message(format!(
                "Hermes event '{other}' is not supported by the Tyde Hermes backend"
            )))]),
        };

        match result {
            Ok(events) => events,
            Err(err) => self.fail_active_turn(err),
        }
    }

    fn fail_active_turn(&mut self, message: impl Into<String>) -> Vec<ChatEvent> {
        let mut events = Vec::new();
        if self.current_message_id.is_some() {
            events.extend(self.finish_stream_events(None, None, None));
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
        if text == "ready" || text.trim().is_empty() {
            return Ok(Vec::new());
        }
        Ok(vec![ChatEvent::MessageAdded(system_message(format!(
            "Hermes {kind}: {text}"
        )))])
    }

    fn map_message_start(&mut self) -> Result<Vec<ChatEvent>, String> {
        let mut events = Vec::new();
        if self.current_message_id.is_some() {
            events.extend(self.finish_stream_events(None, None, None));
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
        if self.awaiting_interrupted_complete
            && status == "interrupted"
            && self.current_message_id.is_none()
        {
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
                ));
                events.extend(self.cancel_events("Operation cancelled"));
            }
            "error" | "failed" => {
                events.extend(self.finish_stream_events(
                    stream_final_text.clone(),
                    usage,
                    cumulative_usage,
                ));
                if let Some(error_text) =
                    optional_string(&payload, &["error"]).or_else(|| stream_final_text.clone())
                {
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
        self.turn_tools
            .insert(tool_call_id.clone(), tool_name.clone());
        Ok(vec![ChatEvent::ToolRequest(ToolRequest {
            tool_call_id,
            tool_name,
            tool_type: ToolRequestType::Other { args: payload },
        })])
    }

    fn map_tool_progress(&mut self, payload: Option<Value>) -> Result<Vec<ChatEvent>, String> {
        let payload = required_payload(payload, "tool.progress")?;
        let tool_call_id =
            required_string_any(&payload, &["tool_id", "tool_call_id"], "tool.progress")?;
        let tool_name = optional_string_any(&payload, &["name", "tool_name"])
            .or_else(|| self.pending_tools.get(&tool_call_id).cloned())
            .or_else(|| self.turn_tools.get(&tool_call_id).cloned())
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
        let error = optional_string(&payload, &["error"]).or_else(|| {
            payload
                .get("error")
                .filter(|value| !value.is_null())
                .map(ToString::to_string)
        });
        let success = error.is_none();
        let result = payload
            .get("result")
            .cloned()
            .or_else(|| payload.get("summary").cloned())
            .unwrap_or(Value::Null);
        Ok(vec![ChatEvent::ToolExecutionCompleted(
            ToolExecutionCompletedData {
                tool_call_id,
                tool_name,
                tool_result: ToolExecutionResult::Other { result },
                success,
                error,
            },
        )])
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
        Ok(vec![
            ChatEvent::ToolRequest(ToolRequest {
                tool_call_id,
                tool_name: "approval.request".to_string(),
                tool_type: ToolRequestType::ExitPlanMode {
                    plan: Some(question),
                    plan_path: None,
                },
            }),
            ChatEvent::TypingStatusChanged(false),
        ])
    }

    fn map_error(&mut self, payload: Option<Value>) -> Result<Vec<ChatEvent>, String> {
        let payload = required_payload(payload, "error")?;
        let message = optional_string(&payload, &["message"])
            .or_else(|| optional_string(&payload, &["error"]))
            .unwrap_or_else(|| payload.to_string());
        Ok(self.fail_active_turn(message))
    }

    fn finish_stream_events(
        &mut self,
        final_text: Option<String>,
        usage: Option<TokenUsage>,
        cumulative_usage: Option<TokenUsage>,
    ) -> Vec<ChatEvent> {
        let content = final_text.unwrap_or_else(|| self.current_text.clone());
        let message_id = self.current_message_id.take().map(protocol::ChatMessageId);
        let reasoning = None;
        self.current_text.clear();
        self.current_reasoning_seen = false;
        let turn_usage = usage;
        let token_usage = match (turn_usage, cumulative_usage) {
            (Some(turn), Some(cumulative)) => Some(
                MessageTokenUsage::request_and_turn_known(turn.clone(), turn)
                    .with_cumulative(cumulative),
            ),
            (Some(turn), None) => Some(MessageTokenUsage::request_and_turn_known(
                turn.clone(),
                turn,
            )),
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
                tool_calls: self.tool_uses_for_message(),
                model_info: self.model.clone().map(|model| ModelInfo { model }),
                token_usage,
                context_breakdown: None,
                images: None,
            },
        })]
    }

    fn cancel_events(&mut self, message: &str) -> Vec<ChatEvent> {
        let mut events = Vec::new();
        if self.current_message_id.is_some() {
            events.extend(self.finish_stream_events(None, None, None));
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
            events.push(ChatEvent::ToolExecutionCompleted(
                ToolExecutionCompletedData {
                    tool_call_id,
                    tool_name,
                    tool_result: ToolExecutionResult::Error {
                        short_message: "Cancelled".to_string(),
                        detailed_message: detailed_message.to_string(),
                    },
                    success: false,
                    error: Some("Cancelled".to_string()),
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

    fn tool_uses_for_message(&self) -> Vec<ToolUseData> {
        self.turn_tools
            .iter()
            .map(|(id, name)| ToolUseData {
                id: id.clone(),
                name: name.clone(),
                arguments: Value::Null,
            })
            .collect()
    }

    fn clear_turn_state(&mut self) {
        self.current_message_id = None;
        self.current_text.clear();
        self.current_reasoning_seen = false;
        self.clear_turn_tool_state();
    }

    fn clear_turn_tool_state(&mut self) {
        self.pending_tools.clear();
        self.turn_tools.clear();
        self.pending_approval_tool_id = None;
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
    if !config.startup_mcp_servers.is_empty() {
        return Err("Hermes MCP injection is not enabled because tui_gateway MCP startup parameters have not been verified".to_string());
    }
    if !config.resolved_spawn_config.mcp_servers.is_empty() {
        return Err("Hermes custom MCP servers are not enabled because tui_gateway MCP startup parameters have not been verified".to_string());
    }
    if config.resolved_spawn_config.tool_policy != protocol::ToolPolicy::Unrestricted {
        return Err("Hermes custom tool policies are not enabled because the native gateway policy mapping has not been verified".to_string());
    }
    Ok(())
}

fn build_session_create_params(
    workspace_roots: &[String],
    resolved: &ResolvedSpawnConfig,
    settings: &SessionSettingsValues,
    backend_config: &BackendConfigValues,
) -> Result<Value, String> {
    let cwd = session_cwd(workspace_roots)?;
    let mut params = json!({
        "cols": 80,
        "source": "tyde",
        "cwd": cwd,
        "close_on_disconnect": false,
    });

    if let Some(instructions) = render_combined_spawn_instructions(resolved) {
        params["messages"] = json!([{ "role": "system", "content": instructions }]);
    }

    // Host deep-config defaults are the baseline (lowest precedence). Because
    // the model id is supplied verbatim rather than picked from a locally
    // probed list, this is also the correct model source for remote workspaces.
    if let Some(model) = backend_config_text(backend_config, "default_model") {
        params["model"] = Value::String(model.to_string());
    }
    if let Some(provider) = backend_config_text(backend_config, "default_provider") {
        params["provider"] = Value::String(provider.to_string());
    }
    if let Some(base_url) = backend_config_text(backend_config, "api_base_url") {
        params["base_url"] = Value::String(base_url.to_string());
    }

    // Per-session model selection overrides the host default.
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

fn session_settings_schema_from_model_options(
    value: &Value,
) -> Result<SessionSettingsSchema, String> {
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
            if !authenticated {
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
        return Err(
            "Hermes model.options reported no authenticated providers with selectable models"
                .to_string(),
        );
    }

    let mut fields = Vec::new();
    fields.push(SessionSettingField {
        key: "model".to_string(),
        label: "Model".to_string(),
        description: Some(
            "Hermes model from authenticated providers reported by model.options.".to_string(),
        ),
        use_slider: false,
        field_type: SessionSettingFieldType::Select {
            options: model_options,
            default: model_default,
            nullable: true,
        },
    });
    fields.extend(hermes_base_session_fields());

    Ok(SessionSettingsSchema {
        backend_kind: BackendKind::Hermes,
        fields,
    })
}

fn parse_session_create_ids(value: &Value) -> Result<HermesSessionIds, String> {
    let live_session_id = required_string(value, &["session_id"], "session.create")?;
    let stored_session_id = required_string(value, &["stored_session_id"], "session.create")?;
    Ok(HermesSessionIds {
        live_session_id,
        stored_session_id: SessionId(stored_session_id),
    })
}

fn parse_session_list(value: &Value) -> Result<Vec<BackendSession>, String> {
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
            resumable: true,
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
    for message in messages {
        let role = required_string(message, &["role"], "session.history message")?;
        let text = optional_string(message, &["text"])
            .or_else(|| optional_string(message, &["content"]))
            .ok_or_else(|| "Hermes session.history message missing text".to_string())?;
        let sender = match role.as_str() {
            "user" => MessageSender::User,
            "assistant" => MessageSender::Assistant {
                agent: HERMES_AGENT_NAME.to_string(),
            },
            "system" => MessageSender::System,
            "tool" => MessageSender::System,
            other => {
                return Err(format!(
                    "Hermes session.history message has unsupported role '{other}'"
                ));
            }
        };
        events.push(ChatEvent::MessageAdded(ChatMessage {
            message_id: None,
            timestamp: unix_now_ms(),
            sender,
            content: text,
            reasoning: None,
            tool_calls: Vec::new(),
            model_info: None,
            token_usage: None,
            context_breakdown: None,
            images: None,
        }));
    }
    Ok(events)
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
            cwd,
            remote_host: Some(host),
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
        display_program: format!("{program} -m {HERMES_PYTHON_MODULE}"),
        program,
        args: vec!["-m".to_string(), HERMES_PYTHON_MODULE.to_string()],
        cwd: Some(session_cwd(workspace_roots)?),
        remote_host: None,
    })
}

async fn resolve_hermes_cli_gateway_spawn_target(
    workspace_roots: &[String],
) -> Result<HermesSpawnTarget, String> {
    if let Some(candidate) = explicit_hermes_executable() {
        return match probe_hermes_cli_gateway(&candidate).await {
            Ok(probe) => {
                let display_program = format!(
                    "{} via {} -m {}",
                    probe.executable, probe.gateway_python, HERMES_PYTHON_MODULE
                );
                Ok(HermesSpawnTarget {
                    program: probe.gateway_python,
                    args: vec!["-m".to_string(), HERMES_PYTHON_MODULE.to_string()],
                    cwd: Some(session_cwd(workspace_roots)?),
                    remote_host: None,
                    display_program,
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
                    "{} via {} -m {}",
                    probe.executable, probe.gateway_python, HERMES_PYTHON_MODULE
                );
                return Ok(HermesSpawnTarget {
                    program: probe.gateway_python,
                    args: vec!["-m".to_string(), HERMES_PYTHON_MODULE.to_string()],
                    cwd: Some(session_cwd(workspace_roots)?),
                    remote_host: None,
                    display_program,
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

fn token_usage_from_value(value: &Value) -> Option<TokenUsage> {
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
    if input_tokens == 0 && output_tokens == 0 && total_tokens == 0 {
        return None;
    }
    Some(TokenUsage {
        input_tokens,
        output_tokens,
        total_tokens,
        cached_prompt_tokens: value.get("cached_prompt_tokens").and_then(Value::as_u64),
        cache_creation_input_tokens: value
            .get("cache_creation_input_tokens")
            .and_then(Value::as_u64),
        reasoning_tokens: value.get("reasoning_tokens").and_then(Value::as_u64),
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

fn token_usage_delta(previous: Option<&TokenUsage>, current: &TokenUsage) -> TokenUsage {
    let Some(previous) = previous else {
        return current.clone();
    };
    TokenUsage {
        input_tokens: current.input_tokens.saturating_sub(previous.input_tokens),
        output_tokens: current.output_tokens.saturating_sub(previous.output_tokens),
        total_tokens: current.total_tokens.saturating_sub(previous.total_tokens),
        cached_prompt_tokens: optional_token_delta(
            previous.cached_prompt_tokens,
            current.cached_prompt_tokens,
        ),
        cache_creation_input_tokens: optional_token_delta(
            previous.cache_creation_input_tokens,
            current.cache_creation_input_tokens,
        ),
        reasoning_tokens: optional_token_delta(previous.reasoning_tokens, current.reasoning_tokens),
    }
}

fn optional_token_delta(previous: Option<u64>, current: Option<u64>) -> Option<u64> {
    current.map(|current| current.saturating_sub(previous.unwrap_or(0)))
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

    fn payload(message: &str) -> SendMessagePayload {
        SendMessagePayload {
            message: message.to_string(),
            images: None,
            origin: None,
            tool_response: None,
        }
    }

    fn write_fake_gateway(dir: &TempDir, body: &str) -> String {
        let script = dir.path().join("fake_gateway.py");
        fs::write(&script, body).expect("write fake gateway");
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
        assert_eq!(
            target.args,
            vec!["-m".to_string(), HERMES_PYTHON_MODULE.to_string()]
        );
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
        sessions[sid] = "stored1"
        print(json.dumps({"jsonrpc":"2.0","id":rid,"result":{"session_id":sid,"stored_session_id":"stored1","messages":[],"info":{}}}), flush=True)
        emit("session.info", sid, {"model":"fake-model","provider":"fake","cwd":"/tmp"})
    elif method == "prompt.submit":
        sid = params["session_id"]
        print(json.dumps({"jsonrpc":"2.0","id":rid,"result":{"status":"streaming"}}), flush=True)
        emit("message.start", sid)
        emit("reasoning.delta", sid, {"text":"think"})
        emit("message.delta", sid, {"text":"hel"})
        emit("message.delta", sid, {"text":"lo"})
        emit("message.complete", sid, {"text":"hello","status":"complete"})
    elif method == "session.interrupt":
        print(json.dumps({"jsonrpc":"2.0","id":rid,"result":{"status":"interrupted"}}), flush=True)
    elif method == "session.usage":
        print(json.dumps({"jsonrpc":"2.0","id":rid,"result":{"input":1,"output":2,"total":3}}), flush=True)
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
                    assert_eq!(
                        end.message
                            .token_usage
                            .expect("usage")
                            .turn
                            .known_usage()
                            .expect("turn usage")
                            .total_tokens,
                        3
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
        backend.shutdown().await;
    }

    #[tokio::test]
    async fn hermes_rejects_images_and_mcp_until_verified() {
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

        let config = BackendSpawnConfig {
            startup_mcp_servers: vec![crate::backend::StartupMcpServer {
                name: "server".to_string(),
                transport: crate::backend::StartupMcpTransport::Http {
                    url: "http://localhost".to_string(),
                    headers: HashMap::new(),
                    bearer_token_env_var: None,
                },
            }],
            ..BackendSpawnConfig::default()
        };
        let err = match HermesBackend::spawn(Vec::new(), config, payload("hello")).await {
            Ok(_) => panic!("MCP injection should be disabled"),
            Err(err) => err,
        };
        assert!(err.contains("MCP injection"));
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

        let schema = probe_session_settings_schema(&[dir.path().to_string_lossy().to_string()])
            .await
            .expect("schema");

        assert!(
            schema.fields.iter().any(|field| field.key == "model"),
            "dynamic Hermes schema must include model options: {schema:?}"
        );
    }

    #[tokio::test]
    async fn hermes_backend_config_snapshot_uses_model_options() {
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
            "providers":[]
        }}), flush=True)
    else:
        print(json.dumps({"jsonrpc":"2.0","id":rid,"result":{}}), flush=True)
"#,
        );
        let _guard = TestHermesPythonGuard::set(&fake);

        let values = probe_backend_config_snapshot(&[dir.path().to_string_lossy().to_string()])
            .await
            .expect("backend config snapshot");

        assert_eq!(
            values.0.get("default_model"),
            Some(&SessionSettingValue::String(
                "anthropic/claude-haiku-4.5".to_string()
            ))
        );
        assert_eq!(
            values.0.get("default_provider"),
            Some(&SessionSettingValue::String("openrouter".to_string()))
        );
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
        timeout(Duration::from_secs(2), async {
            while let Some(event) = events.recv().await {
                if matches!(event, ChatEvent::StreamEnd(_)) {
                    break;
                }
            }
        })
        .await
        .expect("turn should finish");
        backend.shutdown().await;

        let cwd = fs::read_to_string(&cwd_log).expect("read cwd log");
        assert!(
            cwd.ends_with(".tyde/hermes/no-root"),
            "empty-root gateway cwd must be Tyde-owned no-root dir, got {cwd}"
        );
    }

    #[test]
    fn hermes_read_only_instructions_are_seeded_as_system_history() {
        let resolved = ResolvedSpawnConfig {
            access_mode: BackendAccessMode::ReadOnly,
            ..ResolvedSpawnConfig::default()
        };
        let params = build_session_create_params(
            &[],
            &resolved,
            &SessionSettingsValues::default(),
            &BackendConfigValues::default(),
        )
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
        let message = params["messages"][0]["content"]
            .as_str()
            .expect("system seed");
        assert!(message.contains("Backend access mode is read-only"));
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

        let params = build_session_create_params(
            &[],
            &ResolvedSpawnConfig::default(),
            &settings,
            &BackendConfigValues::default(),
        )
        .expect("params");

        assert_eq!(params["model"], "minimax/minimax-m2.7");
        assert_eq!(params["provider"], "openrouter");
        assert_eq!(params["reasoning_effort"], "none");
        assert_eq!(params["fast"], true);
    }

    #[test]
    fn hermes_backend_config_defaults_apply_and_session_setting_overrides() {
        let mut backend_config = BackendConfigValues::default();
        backend_config.0.insert(
            "default_model".to_string(),
            SessionSettingValue::String("anthropic/claude-sonnet-5".to_string()),
        );
        backend_config.0.insert(
            "default_provider".to_string(),
            SessionSettingValue::String("openrouter".to_string()),
        );
        backend_config.0.insert(
            "api_base_url".to_string(),
            SessionSettingValue::String("https://example.test/v1".to_string()),
        );

        // No per-session model: host deep-config defaults apply verbatim.
        let params = build_session_create_params(
            &[],
            &ResolvedSpawnConfig::default(),
            &SessionSettingsValues::default(),
            &backend_config,
        )
        .expect("params");
        assert_eq!(params["model"], "anthropic/claude-sonnet-5");
        assert_eq!(params["provider"], "openrouter");
        assert_eq!(params["base_url"], "https://example.test/v1");

        // Per-session model overrides the configured default model/provider,
        // but the base URL from config still applies.
        let mut settings = SessionSettingsValues::default();
        settings.0.insert(
            "model".to_string(),
            SessionSettingValue::String(encode_model_option_value(
                "minimax/minimax-m2.7",
                Some("minimax"),
            )),
        );
        let params = build_session_create_params(
            &[],
            &ResolvedSpawnConfig::default(),
            &settings,
            &backend_config,
        )
        .expect("params");
        assert_eq!(params["model"], "minimax/minimax-m2.7");
        assert_eq!(params["provider"], "minimax");
        assert_eq!(params["base_url"], "https://example.test/v1");
    }

    #[test]
    fn hermes_backend_config_schema_exposes_model_provider_base_url() {
        let schema = hermes_backend_config_schema();
        assert_eq!(schema.backend_kind, BackendKind::Hermes);
        assert_eq!(
            schema.persistence_mode,
            BackendConfigPersistenceMode::TydeSettingsStore
        );
        let keys: Vec<&str> = schema.fields.iter().map(|f| f.key.as_str()).collect();
        assert_eq!(
            keys,
            vec!["default_model", "default_provider", "api_base_url"]
        );
        assert!(
            schema
                .fields
                .iter()
                .all(|f| matches!(f.field_type, BackendConfigFieldType::Text { .. }))
        );
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

    #[test]
    fn hermes_model_options_schema_uses_authenticated_provider_models() {
        let schema = session_settings_schema_from_model_options(&json!({
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
        let schema = session_settings_schema_from_model_options(&json!({
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
            let err = match session_settings_schema_from_model_options(&payload) {
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
            let err = match session_settings_schema_from_model_options(&payload) {
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
    fn hermes_token_usage_delta_subtracts_cumulative_counts() {
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

        let delta = token_usage_delta(Some(&previous), &current);

        assert_eq!(delta.input_tokens, 8);
        assert_eq!(delta.output_tokens, 8);
        assert_eq!(delta.total_tokens, 16);
        assert_eq!(delta.cached_prompt_tokens, Some(5));
        assert_eq!(delta.cache_creation_input_tokens, Some(5));
        assert_eq!(delta.reasoning_tokens, Some(5));
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
}
