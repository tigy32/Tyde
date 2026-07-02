use std::fs;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use command_group::AsyncCommandGroup;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

use protocol::{
    AgentInput, BackendAccessMode, BackendConfigField, BackendConfigFieldType, BackendConfigSchema,
    BackendConfigValues, BackendKind, ChatEvent, ChatMessage, MessageSender, SelectOption,
    SessionId, SessionSettingValue, StreamEndData, StreamTextDeltaData,
};

use super::{
    Backend, BackendSession, BackendSpawnConfig, BackendStartupError, EventStream,
    StartupMcpServer, StartupMcpTransport, backend_fork_unsupported_message,
    empty_session_settings_schema, render_combined_spawn_instructions,
    setup::resolve_tycode_binary_path,
};
use crate::process_env;

fn subprocess_bin() -> Result<String, String> {
    #[cfg(test)]
    if let Some(path) = TEST_TYCODE_SUBPROCESS_BIN
        .lock()
        .expect("test Tycode subprocess bin mutex poisoned")
        .clone()
    {
        return Ok(path);
    }

    resolve_tycode_binary_path().ok_or_else(|| "tycode-subprocess not found".to_string())
}

#[cfg(test)]
static TEST_TYCODE_SUBPROCESS_BIN: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
#[cfg(test)]
static TEST_TYCODE_SESSIONS_DIR: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);
#[cfg(test)]
static TEST_TYCODE_STARTUP_TIMEOUT: std::sync::Mutex<Option<Duration>> =
    std::sync::Mutex::new(None);

fn tycode_startup_timeout() -> Duration {
    #[cfg(test)]
    if let Some(timeout) = *TEST_TYCODE_STARTUP_TIMEOUT
        .lock()
        .expect("test Tycode startup timeout mutex poisoned")
    {
        return timeout;
    }

    Duration::from_secs(30)
}

pub struct TycodeBackend {
    input_tx: mpsc::UnboundedSender<AgentInput>,
    interrupt_tx: mpsc::UnboundedSender<()>,
    shutdown_tx: mpsc::UnboundedSender<()>,
    session_id: Arc<std::sync::Mutex<Option<SessionId>>>,
}

enum TycodeStdinCommand {
    Json(Value),
    Cancel,
}

struct TempWorkspaceRoot {
    path: PathBuf,
}

impl TempWorkspaceRoot {
    fn new(prefix: &str) -> Result<Self, String> {
        let path = std::env::temp_dir().join(format!("{prefix}-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).map_err(|err| {
            format!(
                "Failed to create temporary workspace {}: {err}",
                path.display()
            )
        })?;
        Ok(Self { path })
    }
}

impl Drop for TempWorkspaceRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn write_text_file(path: &PathBuf, body: &str) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("Path has no parent: {}", path.display()))?;
    fs::create_dir_all(parent)
        .map_err(|err| format!("Failed to create directory {}: {err}", parent.display()))?;
    fs::write(path, body).map_err(|err| format!("Failed to write {}: {err}", path.display()))
}

fn materialize_tycode_customization(
    config: &BackendSpawnConfig,
) -> Result<Option<TempWorkspaceRoot>, String> {
    let steering = render_combined_spawn_instructions(&config.resolved_spawn_config);
    if steering.is_none() && config.resolved_spawn_config.skills.is_empty() {
        return Ok(None);
    }
    let root = TempWorkspaceRoot::new("tyde-tycode-customization")?;
    if let Some(steering) = steering {
        write_text_file(
            &root.path.join(".tycode").join("tyde_steering.md"),
            &steering,
        )?;
    }
    for skill in &config.resolved_spawn_config.skills {
        write_text_file(
            &root
                .path
                .join(".tycode")
                .join("skills")
                .join(&skill.name)
                .join("SKILL.md"),
            &skill.body,
        )?;
    }
    Ok(Some(root))
}

fn tycode_read_only_agent_json(config: &BackendSpawnConfig) -> Option<String> {
    if config.resolved_spawn_config.access_mode != BackendAccessMode::ReadOnly {
        return None;
    }
    let system_prompt = render_combined_spawn_instructions(&config.resolved_spawn_config)
        .unwrap_or_else(|| {
            "Backend access mode is read-only: inspect files and call configured MCP tools only."
                .to_string()
        });
    Some(
        serde_json::json!({
            "name": "tyde-read-only",
            "description": "Tyde read-only agent",
            "systemPrompt": system_prompt,
            "tools": [
                "set_tracked_files",
                "search_types",
                "get_type_docs",
                "run_build_test"
            ]
        })
        .to_string(),
    )
}

fn tycode_backend_config_schema() -> BackendConfigSchema {
    BackendConfigSchema {
        backend_kind: BackendKind::Tycode,
        fields: vec![
            BackendConfigField {
                key: "active_provider".to_string(),
                label: "Active Provider".to_string(),
                description: Some(
                    "Existing Tycode provider name to activate for new sessions. Tyde validates \
                     the name against Tycode's returned providers and never edits provider \
                     secrets or creates providers."
                        .to_string(),
                ),
                field_type: BackendConfigFieldType::Text {
                    default: None,
                    placeholder: Some("default".to_string()),
                    multiline: false,
                },
            },
            BackendConfigField {
                key: "model_quality".to_string(),
                label: "Model Quality".to_string(),
                description: Some(
                    "Global Tycode model cost/quality ceiling. Auto leaves the Tycode setting \
                     unchanged."
                        .to_string(),
                ),
                field_type: BackendConfigFieldType::Select {
                    options: vec![
                        select_option("free", "Free"),
                        select_option("low", "Low"),
                        select_option("medium", "Medium"),
                        select_option("high", "High"),
                        select_option("unlimited", "Unlimited"),
                    ],
                    default: None,
                    nullable: true,
                },
            },
            BackendConfigField {
                key: "reasoning_effort".to_string(),
                label: "Reasoning Effort".to_string(),
                description: Some(
                    "Global Tycode reasoning effort. Auto leaves the Tycode setting unchanged."
                        .to_string(),
                ),
                field_type: BackendConfigFieldType::Select {
                    options: vec![
                        select_option("Off", "Off"),
                        select_option("Low", "Low"),
                        select_option("Medium", "Medium"),
                        select_option("High", "High"),
                        select_option("Max", "Max"),
                    ],
                    default: None,
                    nullable: true,
                },
            },
            BackendConfigField {
                key: "autonomy_level".to_string(),
                label: "Autonomy Level".to_string(),
                description: Some(
                    "Controls whether Tycode must ask for plan approval before implementing. \
                     Auto leaves the Tycode setting unchanged."
                        .to_string(),
                ),
                field_type: BackendConfigFieldType::Select {
                    options: vec![
                        select_option("plan_approval_required", "Plan approval required"),
                        select_option("fully_autonomous", "Fully autonomous"),
                    ],
                    default: None,
                    nullable: true,
                },
            },
            BackendConfigField {
                key: "review_level".to_string(),
                label: "Review Level".to_string(),
                description: Some(
                    "Controls Tycode's built-in review-agent behavior for completed coder tasks. \
                     Auto leaves the Tycode setting unchanged."
                        .to_string(),
                ),
                field_type: BackendConfigFieldType::Select {
                    options: vec![select_option("None", "None"), select_option("Task", "Task")],
                    default: None,
                    nullable: true,
                },
            },
            BackendConfigField {
                key: "spawn_context_mode".to_string(),
                label: "Spawn Context Mode".to_string(),
                description: Some(
                    "Controls whether spawned Tycode sub-agents inherit parent conversation \
                     context. Auto leaves the Tycode setting unchanged."
                        .to_string(),
                ),
                field_type: BackendConfigFieldType::Select {
                    options: vec![
                        select_option("Fork", "Fork"),
                        select_option("Fresh", "Fresh"),
                    ],
                    default: None,
                    nullable: true,
                },
            },
        ],
    }
}

fn select_option(value: &str, label: &str) -> SelectOption {
    SelectOption {
        value: value.to_string(),
        label: label.to_string(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TycodeSettingsOverlay {
    settings: Value,
    active_provider_change: Option<String>,
}

fn apply_tycode_backend_config_overlay(
    current_settings: &Value,
    config: &BackendConfigValues,
) -> Result<TycodeSettingsOverlay, String> {
    let mut settings = current_settings.clone();
    let object = settings
        .as_object_mut()
        .ok_or_else(|| "Tycode Settings event data must be a JSON object".to_string())?;

    let mut active_provider_change = None;
    for (key, value) in &config.0 {
        match (key.as_str(), value) {
            ("active_provider", SessionSettingValue::String(provider)) => {
                let provider = provider.trim();
                if provider.is_empty() {
                    return Err("Tycode active_provider must not be empty".to_string());
                }
                let providers = object
                    .get("providers")
                    .and_then(Value::as_object)
                    .ok_or_else(|| {
                        "Tycode settings missing providers object while validating active_provider"
                            .to_string()
                    })?;
                if !providers.contains_key(provider) {
                    let available = providers.keys().cloned().collect::<Vec<_>>().join(", ");
                    return Err(format!(
                        "Configured Tycode active_provider '{provider}' is absent from returned providers{}",
                        if available.is_empty() {
                            String::new()
                        } else {
                            format!(" (available: {available})")
                        }
                    ));
                }
                active_provider_change = Some(provider.to_string());
                object.insert(
                    "active_provider".to_string(),
                    Value::String(provider.to_string()),
                );
            }
            ("model_quality", SessionSettingValue::String(model_quality)) => {
                object.insert(
                    "model_quality".to_string(),
                    Value::String(model_quality.clone()),
                );
            }
            ("model_quality", SessionSettingValue::Null) => {
                continue;
            }
            ("reasoning_effort", SessionSettingValue::String(reasoning_effort)) => {
                object.insert(
                    "reasoning_effort".to_string(),
                    Value::String(reasoning_effort.clone()),
                );
            }
            ("reasoning_effort", SessionSettingValue::Null) => {
                continue;
            }
            (
                "autonomy_level" | "review_level" | "spawn_context_mode",
                SessionSettingValue::String(setting),
            ) => {
                object.insert(key.clone(), Value::String(setting.clone()));
            }
            (
                "autonomy_level" | "review_level" | "spawn_context_mode",
                SessionSettingValue::Null,
            ) => {
                continue;
            }
            ("active_provider", _) => {
                return Err("Tycode active_provider backend config must be a string".to_string());
            }
            ("model_quality" | "reasoning_effort", _) => {
                return Err(format!(
                    "Tycode {key} backend config must be a string or null"
                ));
            }
            ("autonomy_level" | "review_level" | "spawn_context_mode", _) => {
                return Err(format!(
                    "Tycode {key} backend config must be a string or null"
                ));
            }
            _ => {}
        }
    }

    Ok(TycodeSettingsOverlay {
        settings,
        active_provider_change,
    })
}

enum TycodeStartupFollowUp {
    InitialUserInput(String),
    ResumeSession { session_id: String },
}

enum TycodeStartupPhase {
    AwaitSessionStarted,
    AwaitInitialSettings,
    AwaitVerification {
        expected_settings: Value,
        active_provider_change: Option<String>,
    },
    AwaitProviderChange {
        provider: String,
    },
    Complete,
}

enum TycodeStartupObservation {
    Allow,
    Suppress,
    Completed,
}

struct TycodeStartupController {
    backend_config: BackendConfigValues,
    phase: TycodeStartupPhase,
    follow_up: TycodeStartupFollowUp,
}

impl TycodeStartupController {
    fn new(backend_config: BackendConfigValues, follow_up: TycodeStartupFollowUp) -> Self {
        Self {
            backend_config,
            phase: TycodeStartupPhase::AwaitSessionStarted,
            follow_up,
        }
    }

    fn observe(
        &mut self,
        value: &Value,
        stdin_tx: &mpsc::UnboundedSender<TycodeStdinCommand>,
    ) -> Result<TycodeStartupObservation, String> {
        match &mut self.phase {
            TycodeStartupPhase::AwaitSessionStarted => {
                if tycode_session_started(value).is_some() {
                    if self.backend_config.0.is_empty() {
                        self.send_follow_up(stdin_tx)?;
                        self.phase = TycodeStartupPhase::Complete;
                        return Ok(TycodeStartupObservation::Completed);
                    }
                    send_tycode_json(stdin_tx, Value::String("GetSettings".to_string()))?;
                    self.phase = TycodeStartupPhase::AwaitInitialSettings;
                }
                Ok(TycodeStartupObservation::Allow)
            }
            TycodeStartupPhase::AwaitInitialSettings => {
                if let Some(error) = tycode_error_message(value) {
                    return Err(format!(
                        "Tycode settings initialization failed before Settings: {error}"
                    ));
                }
                if let Some(settings) = tycode_settings_data(value) {
                    let overlay =
                        apply_tycode_backend_config_overlay(settings, &self.backend_config)
                            .map_err(|err| {
                                format!("Failed to apply Tycode settings overlay: {err}")
                            })?;
                    send_tycode_json(
                        stdin_tx,
                        serde_json::json!({
                            "SaveSettings": {
                                "settings": overlay.settings.clone(),
                                "persist": false,
                            }
                        }),
                    )?;
                    send_tycode_json(stdin_tx, Value::String("GetSettings".to_string()))?;
                    self.phase = TycodeStartupPhase::AwaitVerification {
                        expected_settings: overlay.settings,
                        active_provider_change: overlay.active_provider_change,
                    };
                    return Ok(TycodeStartupObservation::Suppress);
                }
                Ok(tycode_startup_internal_observation(value))
            }
            TycodeStartupPhase::AwaitVerification {
                expected_settings,
                active_provider_change,
            } => {
                if let Some(error) = tycode_error_message(value) {
                    return Err(format!(
                        "Tycode settings SaveSettings/verification failed: {error}"
                    ));
                }
                if let Some(settings) = tycode_settings_data(value) {
                    verify_tycode_settings_overlay(expected_settings, settings)?;
                    if let Some(provider) = active_provider_change.take() {
                        send_tycode_json(
                            stdin_tx,
                            serde_json::json!({ "ChangeProvider": provider }),
                        )?;
                        self.phase = TycodeStartupPhase::AwaitProviderChange { provider };
                    } else {
                        self.send_follow_up(stdin_tx)?;
                        self.phase = TycodeStartupPhase::Complete;
                        return Ok(TycodeStartupObservation::Completed);
                    }
                    return Ok(TycodeStartupObservation::Suppress);
                }
                Ok(tycode_startup_internal_observation(value))
            }
            TycodeStartupPhase::AwaitProviderChange { provider } => {
                if let Some(error) = tycode_error_message(value) {
                    return Err(format!(
                        "Tycode ChangeProvider '{provider}' failed: {error}"
                    ));
                }
                if tycode_provider_changed_message(value, provider) {
                    self.send_follow_up(stdin_tx)?;
                    self.phase = TycodeStartupPhase::Complete;
                    return Ok(TycodeStartupObservation::Completed);
                }
                Ok(tycode_startup_internal_observation(value))
            }
            TycodeStartupPhase::Complete => Ok(TycodeStartupObservation::Allow),
        }
    }

    fn send_follow_up(
        &self,
        stdin_tx: &mpsc::UnboundedSender<TycodeStdinCommand>,
    ) -> Result<(), String> {
        match &self.follow_up {
            TycodeStartupFollowUp::InitialUserInput(message) => {
                send_tycode_json(stdin_tx, serde_json::json!({ "UserInput": message }))
            }
            TycodeStartupFollowUp::ResumeSession { session_id } => {
                send_tycode_json(
                    stdin_tx,
                    serde_json::json!({
                        "ResumeSession": { "session_id": session_id }
                    }),
                )?;
                send_tycode_json(stdin_tx, Value::String("ListSessions".to_string()))
            }
        }
    }

    fn phase_description(&self) -> &'static str {
        match self.phase {
            TycodeStartupPhase::AwaitSessionStarted => "waiting for SessionStarted",
            TycodeStartupPhase::AwaitInitialSettings => "waiting for Settings after GetSettings",
            TycodeStartupPhase::AwaitVerification { .. } => {
                "waiting for Settings verification after SaveSettings"
            }
            TycodeStartupPhase::AwaitProviderChange { .. } => {
                "waiting for ChangeProvider acknowledgement"
            }
            TycodeStartupPhase::Complete => "complete",
        }
    }
}

type TycodeStartupStatus = Arc<std::sync::Mutex<&'static str>>;

fn new_tycode_startup_status() -> TycodeStartupStatus {
    Arc::new(std::sync::Mutex::new("waiting for task start"))
}

fn set_tycode_startup_status(status: &TycodeStartupStatus, phase: &'static str) {
    *status.lock().expect("tycode startup status mutex poisoned") = phase;
}

async fn await_tycode_startup(
    ready_rx: tokio::sync::oneshot::Receiver<Result<(), String>>,
    shutdown_tx: &mpsc::UnboundedSender<()>,
    operation: &str,
    status: &TycodeStartupStatus,
) -> Result<(), String> {
    let timeout = tycode_startup_timeout();
    match tokio::time::timeout(timeout, ready_rx).await {
        Ok(Ok(Ok(()))) => Ok(()),
        Ok(Ok(Err(err))) => Err(err),
        Ok(Err(_)) => Err(format!(
            "Tycode {operation} initialization task ended early"
        )),
        Err(_) => {
            let _ = shutdown_tx.send(());
            let phase = *status.lock().expect("tycode startup status mutex poisoned");
            Err(format!(
                "Timed out after {} waiting for Tycode {operation} startup/settings handshake: {phase}",
                format_tycode_timeout(timeout)
            ))
        }
    }
}

fn format_tycode_timeout(timeout: Duration) -> String {
    if timeout.as_secs() > 0 {
        format!("{}s", timeout.as_secs())
    } else {
        format!("{}ms", timeout.as_millis())
    }
}

fn send_tycode_json(
    stdin_tx: &mpsc::UnboundedSender<TycodeStdinCommand>,
    value: Value,
) -> Result<(), String> {
    stdin_tx
        .send(TycodeStdinCommand::Json(value))
        .map_err(|_| "Tycode stdin writer closed".to_string())
}

fn tycode_settings_data(value: &Value) -> Option<&Value> {
    (value.get("kind").and_then(Value::as_str) == Some("Settings"))
        .then(|| value.get("data"))
        .flatten()
}

fn tycode_error_message(value: &Value) -> Option<String> {
    if value.get("kind").and_then(Value::as_str) == Some("Error") {
        return value
            .get("data")
            .and_then(Value::as_str)
            .map(str::to_string);
    }
    if value.get("kind").and_then(Value::as_str) != Some("MessageAdded") {
        return None;
    }
    let data = value.get("data")?;
    (data.get("sender").and_then(Value::as_str) == Some("Error")
        || data
            .get("sender")
            .and_then(Value::as_object)
            .is_some_and(|sender| sender.contains_key("Error")))
    .then(|| data.get("content").and_then(Value::as_str))
    .flatten()
    .map(str::to_string)
}

fn tycode_provider_changed_message(value: &Value, provider: &str) -> bool {
    if value.get("kind").and_then(Value::as_str) != Some("MessageAdded") {
        return false;
    }
    let Some(data) = value.get("data") else {
        return false;
    };
    let is_system = data.get("sender").and_then(Value::as_str) == Some("System");
    let expected = format!("Switched to provider: {provider}");
    is_system && data.get("content").and_then(Value::as_str) == Some(expected.as_str())
}

fn tycode_startup_internal_observation(value: &Value) -> TycodeStartupObservation {
    match value.get("kind").and_then(Value::as_str) {
        Some("Settings" | "TimingUpdate" | "TypingStatusChanged") => {
            TycodeStartupObservation::Suppress
        }
        _ => TycodeStartupObservation::Allow,
    }
}

fn tycode_settings_verification_error(expected: &Value, actual: &Value) -> String {
    let managed_keys = [
        "active_provider",
        "model_quality",
        "reasoning_effort",
        "autonomy_level",
        "review_level",
        "spawn_context_mode",
    ];
    let mismatched = managed_keys
        .into_iter()
        .filter(|key| expected.get(*key) != actual.get(*key))
        .collect::<Vec<_>>();
    let providers_changed = expected.get("providers") != actual.get("providers");
    let mut details = Vec::new();
    if !mismatched.is_empty() {
        details.push(format!(
            "mismatched managed keys: {}",
            mismatched.join(", ")
        ));
    }
    if providers_changed {
        details.push("providers changed".to_string());
    }
    if details.is_empty() {
        details.push("returned settings differed outside Tyde-managed fields".to_string());
    }
    format!(
        "Tycode settings verification failed after SaveSettings ({})",
        details.join("; ")
    )
}

fn verify_tycode_settings_overlay(expected: &Value, actual: &Value) -> Result<(), String> {
    let managed_keys = [
        "active_provider",
        "model_quality",
        "reasoning_effort",
        "autonomy_level",
        "review_level",
        "spawn_context_mode",
    ];
    let managed_keys_match = managed_keys
        .into_iter()
        .all(|key| expected.get(key) == actual.get(key));
    let providers_match = expected.get("providers") == actual.get("providers");
    if managed_keys_match && providers_match {
        Ok(())
    } else {
        Err(tycode_settings_verification_error(expected, actual))
    }
}

impl Backend for TycodeBackend {
    fn session_settings_schema() -> protocol::SessionSettingsSchema {
        empty_session_settings_schema(BackendKind::Tycode)
    }

    fn backend_config_schema() -> Option<BackendConfigSchema> {
        Some(tycode_backend_config_schema())
    }

    async fn spawn(
        workspace_roots: Vec<String>,
        config: BackendSpawnConfig,
        initial_input: protocol::SendMessagePayload,
    ) -> Result<(Self, EventStream), String> {
        let initial_message = initial_input.message;
        let (input_tx, mut input_rx) = mpsc::unbounded_channel::<AgentInput>();
        let (interrupt_tx, mut interrupt_rx) = mpsc::unbounded_channel::<()>();
        let (shutdown_tx, mut shutdown_rx) = mpsc::unbounded_channel::<()>();
        let (events_tx, events_rx) = mpsc::unbounded_channel::<ChatEvent>();
        let session_id = Arc::new(std::sync::Mutex::new(None));
        let session_id_task = Arc::clone(&session_id);
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<Result<(), String>>();
        let mcp_servers_json = build_tycode_mcp_servers_json(&config.startup_mcp_servers);
        let startup_status = new_tycode_startup_status();
        let startup_status_task = Arc::clone(&startup_status);

        tokio::spawn(async move {
            let materialized_customization = match materialize_tycode_customization(&config) {
                Ok(root) => root,
                Err(err) => {
                    tracing::error!("Failed to materialize Tycode customization: {err}");
                    let _ = ready_tx.send(Err(err));
                    return;
                }
            };
            let mut workspace_roots = workspace_roots;
            if let Some(root) = materialized_customization.as_ref() {
                workspace_roots.push(root.path.to_string_lossy().to_string());
            }
            let roots_json = serde_json::json!(workspace_roots).to_string();
            let subprocess_bin = match subprocess_bin() {
                Ok(path) => path,
                Err(err) => {
                    tracing::error!("{err}");
                    let _ = ready_tx.send(Err(err));
                    return;
                }
            };
            let mut command = Command::new(&subprocess_bin);
            command.arg("--workspace-roots").arg(&roots_json);
            if let Some(agent_json) = tycode_read_only_agent_json(&config) {
                command.arg("--agent").arg(agent_json);
            }
            if let Some(mcp_servers_json) = mcp_servers_json.as_deref() {
                command.arg("--mcp-servers").arg(mcp_servers_json);
            }
            if let Some(path) = process_env::resolved_child_process_path() {
                command.env("PATH", path);
            }
            command
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());

            let mut child = match command.group_spawn() {
                Ok(c) => c,
                Err(err) => {
                    tracing::error!("Failed to spawn tycode-subprocess: {err}");
                    let _ = ready_tx.send(Err(format!("Failed to spawn tycode-subprocess: {err}")));
                    return;
                }
            };

            let stdin = match child.inner().stdin.take() {
                Some(s) => s,
                None => {
                    tracing::error!("Failed to capture tycode-subprocess stdin");
                    let _ =
                        ready_tx.send(Err("Failed to capture tycode-subprocess stdin".to_string()));
                    return;
                }
            };
            let stdout = match child.inner().stdout.take() {
                Some(s) => s,
                None => {
                    tracing::error!("Failed to capture tycode-subprocess stdout");
                    let _ = ready_tx
                        .send(Err("Failed to capture tycode-subprocess stdout".to_string()));
                    return;
                }
            };
            let stderr = match child.inner().stderr.take() {
                Some(s) => s,
                None => {
                    tracing::error!("Failed to capture tycode-subprocess stderr");
                    let _ = ready_tx
                        .send(Err("Failed to capture tycode-subprocess stderr".to_string()));
                    return;
                }
            };
            let last_stderr_line = spawn_tycode_stderr_logger(stderr);

            // Spawn a task to forward follow-up messages to stdin
            let (stdin_tx, mut stdin_rx) = mpsc::unbounded_channel::<TycodeStdinCommand>();
            tokio::spawn(async move {
                let mut stdin = stdin;
                while let Some(command) = stdin_rx.recv().await {
                    let ok = match command {
                        TycodeStdinCommand::Json(command) => {
                            write_command(&mut stdin, &command).await
                        }
                        TycodeStdinCommand::Cancel => write_cancel(&mut stdin).await,
                    };
                    if !ok {
                        break;
                    }
                }
            });

            let mut startup = TycodeStartupController::new(
                config.backend_config.clone(),
                TycodeStartupFollowUp::InitialUserInput(initial_message),
            );
            set_tycode_startup_status(&startup_status_task, startup.phase_description());

            // Forward AgentInput to the stdin writer
            let stdin_tx2 = stdin_tx.clone();
            tokio::spawn(async move {
                while let Some(input) = input_rx.recv().await {
                    match input {
                        AgentInput::SendMessage(payload) => {
                            let message = payload.message;
                            if stdin_tx2
                                .send(TycodeStdinCommand::Json(
                                    serde_json::json!({ "UserInput": message }),
                                ))
                                .is_err()
                            {
                                break;
                            }
                        }
                        AgentInput::UpdateSessionSettings(_) => {}
                        AgentInput::EditQueuedMessage(_)
                        | AgentInput::CancelQueuedMessage(_)
                        | AgentInput::SendQueuedMessageNow(_) => {
                            panic!(
                                "queued-message inputs must be handled by the agent actor before reaching the backend"
                            );
                        }
                    }
                }
            });

            let stdin_tx_interrupt = stdin_tx.clone();
            tokio::spawn(async move {
                while interrupt_rx.recv().await.is_some() {
                    if stdin_tx_interrupt.send(TycodeStdinCommand::Cancel).is_err() {
                        break;
                    }
                }
            });

            // Read stdout line by line — the subprocess emits ChatEvent JSON directly
            let mut lines = BufReader::new(stdout).lines();
            let mut stream_open = false;
            let mut accumulated_text = String::new();
            let mut ready_tx = Some(ready_tx);
            loop {
                let line = tokio::select! {
                    line = lines.next_line() => line,
                    shutdown = shutdown_rx.recv() => {
                        if shutdown.is_some() {
                            let _ = child.kill().await;
                        }
                        break;
                    }
                };
                let Ok(Some(line)) = line else {
                    break;
                };
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }

                let value: Value = match serde_json::from_str(trimmed) {
                    Ok(value) => value,
                    Err(err) => {
                        tracing::warn!(
                            "Failed to parse tycode-subprocess event: {err} — line: {trimmed}"
                        );
                        continue;
                    }
                };

                if session_id_task
                    .lock()
                    .expect("tycode session_id mutex poisoned")
                    .is_none()
                    && let Some(session) = tycode_session_started(&value)
                {
                    *session_id_task
                        .lock()
                        .expect("tycode session_id mutex poisoned") = Some(session);
                }

                let observation = match startup.observe(&value, &stdin_tx) {
                    Ok(observation) => observation,
                    Err(err) => {
                        tracing::error!("{err}");
                        if let Some(ready_tx) = ready_tx.take() {
                            let _ = ready_tx.send(Err(err));
                        }
                        let _ = child.kill().await;
                        return;
                    }
                };
                set_tycode_startup_status(&startup_status_task, startup.phase_description());
                match observation {
                    TycodeStartupObservation::Allow => {}
                    TycodeStartupObservation::Suppress => continue,
                    TycodeStartupObservation::Completed => {
                        if let Some(ready_tx) = ready_tx.take() {
                            let _ = ready_tx.send(Ok(()));
                        }
                        continue;
                    }
                }

                let events = map_tycode_value_to_chat_events(&value);
                if events.is_empty() {
                    continue;
                }

                for event in events {
                    update_tycode_stream_state(&event, &mut stream_open, &mut accumulated_text);
                    if events_tx.send(event).is_err() {
                        break;
                    }
                    if events_tx.is_closed() {
                        break;
                    }
                }
            }

            // Some tycode builds terminate without emitting StreamEnd. Synthesize
            // one so downstream callers don't hang waiting for end-of-turn.
            if stream_open {
                let _ = events_tx.send(ChatEvent::StreamEnd(StreamEndData {
                    message: ChatMessage {
                        message_id: None,
                        timestamp: unix_now_ms(),
                        sender: MessageSender::Assistant {
                            agent: "tycode".to_string(),
                        },
                        content: accumulated_text,
                        reasoning: None,
                        tool_calls: Vec::new(),
                        model_info: None,
                        token_usage: None,
                        turn_token_usage: None,
                        context_breakdown: None,
                        images: None,
                    },
                }));
            }

            if let Some(ready_tx) = ready_tx.take() {
                let _ = ready_tx.send(Err(tycode_startup_exit_error(&last_stderr_line)));
            }
        });

        await_tycode_startup(ready_rx, &shutdown_tx, "spawn", &startup_status).await?;

        Ok((
            Self {
                input_tx,
                interrupt_tx,
                shutdown_tx,
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
        let replay_event_count = tycode_resume_replay_event_count(&session_id)?;
        let (input_tx, mut input_rx) = mpsc::unbounded_channel::<AgentInput>();
        let (interrupt_tx, mut interrupt_rx) = mpsc::unbounded_channel::<()>();
        let (shutdown_tx, mut shutdown_rx) = mpsc::unbounded_channel::<()>();
        let (events_tx, events_rx) = mpsc::unbounded_channel::<ChatEvent>();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<Result<(), String>>();
        let (resume_replay_complete_tx, resume_replay_complete_rx) =
            tokio::sync::oneshot::channel();
        let known_session_id = Arc::new(std::sync::Mutex::new(Some(session_id.clone())));
        let mcp_servers_json = build_tycode_mcp_servers_json(&config.startup_mcp_servers);
        let startup_status = new_tycode_startup_status();
        let startup_status_task = Arc::clone(&startup_status);

        tokio::spawn(async move {
            let materialized_customization = match materialize_tycode_customization(&config) {
                Ok(root) => root,
                Err(err) => {
                    tracing::error!("Failed to materialize Tycode resume customization: {err}");
                    let _ = ready_tx.send(Err(err));
                    return;
                }
            };
            let mut workspace_roots = workspace_roots;
            if let Some(root) = materialized_customization.as_ref() {
                workspace_roots.push(root.path.to_string_lossy().to_string());
            }
            let roots_json = serde_json::json!(workspace_roots).to_string();
            let subprocess_bin = match subprocess_bin() {
                Ok(path) => path,
                Err(err) => {
                    tracing::error!("{err}");
                    let _ = ready_tx.send(Err(err));
                    return;
                }
            };
            let mut command = Command::new(&subprocess_bin);
            command.arg("--workspace-roots").arg(&roots_json);
            if let Some(agent_json) = tycode_read_only_agent_json(&config) {
                command.arg("--agent").arg(agent_json);
            }
            if let Some(mcp_servers_json) = mcp_servers_json.as_deref() {
                command.arg("--mcp-servers").arg(mcp_servers_json);
            }
            if let Some(path) = process_env::resolved_child_process_path() {
                command.env("PATH", path);
            }
            command
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());

            let mut child = match command.group_spawn() {
                Ok(c) => c,
                Err(err) => {
                    tracing::error!("Failed to spawn tycode-subprocess for resume: {err}");
                    let _ = ready_tx.send(Err(format!("Failed to spawn tycode-subprocess: {err}")));
                    return;
                }
            };

            let stdin = match child.inner().stdin.take() {
                Some(s) => s,
                None => {
                    tracing::error!("Failed to capture tycode-subprocess stdin for resume");
                    let _ =
                        ready_tx.send(Err("Failed to capture tycode-subprocess stdin".to_string()));
                    return;
                }
            };
            let stdout = match child.inner().stdout.take() {
                Some(s) => s,
                None => {
                    tracing::error!("Failed to capture tycode-subprocess stdout for resume");
                    let _ = ready_tx
                        .send(Err("Failed to capture tycode-subprocess stdout".to_string()));
                    return;
                }
            };
            let stderr = match child.inner().stderr.take() {
                Some(s) => s,
                None => {
                    tracing::error!("Failed to capture tycode-subprocess stderr for resume");
                    let _ = ready_tx
                        .send(Err("Failed to capture tycode-subprocess stderr".to_string()));
                    return;
                }
            };
            let last_stderr_line = spawn_tycode_stderr_logger(stderr);

            let (stdin_tx, mut stdin_rx) = mpsc::unbounded_channel::<TycodeStdinCommand>();
            tokio::spawn(async move {
                let mut stdin = stdin;
                while let Some(command) = stdin_rx.recv().await {
                    let ok = match command {
                        TycodeStdinCommand::Json(command) => {
                            write_command(&mut stdin, &command).await
                        }
                        TycodeStdinCommand::Cancel => write_cancel(&mut stdin).await,
                    };
                    if !ok {
                        break;
                    }
                }
            });

            let mut startup = TycodeStartupController::new(
                config.backend_config.clone(),
                TycodeStartupFollowUp::ResumeSession {
                    session_id: session_id.0.clone(),
                },
            );
            set_tycode_startup_status(&startup_status_task, startup.phase_description());

            let stdin_tx2 = stdin_tx.clone();
            tokio::spawn(async move {
                while let Some(input) = input_rx.recv().await {
                    match input {
                        AgentInput::SendMessage(payload) => {
                            let message = payload.message;
                            if stdin_tx2
                                .send(TycodeStdinCommand::Json(
                                    serde_json::json!({ "UserInput": message }),
                                ))
                                .is_err()
                            {
                                break;
                            }
                        }
                        AgentInput::UpdateSessionSettings(_) => {}
                        AgentInput::EditQueuedMessage(_)
                        | AgentInput::CancelQueuedMessage(_)
                        | AgentInput::SendQueuedMessageNow(_) => {
                            panic!(
                                "queued-message inputs must be handled by the agent actor before reaching the backend"
                            );
                        }
                    }
                }
            });

            let stdin_tx_interrupt = stdin_tx.clone();
            tokio::spawn(async move {
                while interrupt_rx.recv().await.is_some() {
                    if stdin_tx_interrupt.send(TycodeStdinCommand::Cancel).is_err() {
                        break;
                    }
                }
            });

            let mut lines = BufReader::new(stdout).lines();
            let mut stream_open = false;
            let mut accumulated_text = String::new();
            let mut replay_barrier =
                TycodeResumeReplayBarrier::new(session_id.0.clone(), replay_event_count);
            let mut resume_replay_complete_tx = Some(resume_replay_complete_tx);
            let mut ready_tx = Some(ready_tx);
            loop {
                let line = tokio::select! {
                    line = lines.next_line() => line,
                    shutdown = shutdown_rx.recv() => {
                        if shutdown.is_some() {
                            let _ = child.kill().await;
                        }
                        break;
                    }
                };
                let Ok(Some(line)) = line else {
                    break;
                };
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }

                let value: Value = match serde_json::from_str(trimmed) {
                    Ok(value) => value,
                    Err(err) => {
                        tracing::warn!(
                            "Failed to parse tycode-subprocess resume event: {err} — line: {trimmed}"
                        );
                        continue;
                    }
                };

                let observation = match startup.observe(&value, &stdin_tx) {
                    Ok(observation) => observation,
                    Err(err) => {
                        tracing::error!("{err}");
                        if let Some(ready_tx) = ready_tx.take() {
                            let _ = ready_tx.send(Err(err));
                        }
                        let _ = child.kill().await;
                        return;
                    }
                };
                set_tycode_startup_status(&startup_status_task, startup.phase_description());
                match observation {
                    TycodeStartupObservation::Allow => {}
                    TycodeStartupObservation::Suppress => continue,
                    TycodeStartupObservation::Completed => {
                        if let Some(ready_tx) = ready_tx.take() {
                            let _ = ready_tx.send(Ok(()));
                        }
                        continue;
                    }
                }

                if resume_replay_complete_tx.is_some() && replay_barrier.observe(&value) {
                    if let Some(tx) = resume_replay_complete_tx.take() {
                        let _ = tx.send(());
                    }
                    continue;
                }

                let events = map_tycode_value_to_chat_events(&value);
                if events.is_empty() {
                    continue;
                }

                for event in events {
                    update_tycode_stream_state(&event, &mut stream_open, &mut accumulated_text);
                    if events_tx.send(event).is_err() {
                        break;
                    }
                }
            }

            if stream_open {
                let _ = events_tx.send(ChatEvent::StreamEnd(StreamEndData {
                    message: ChatMessage {
                        message_id: None,
                        timestamp: unix_now_ms(),
                        sender: MessageSender::Assistant {
                            agent: "tycode".to_string(),
                        },
                        content: accumulated_text,
                        reasoning: None,
                        tool_calls: Vec::new(),
                        model_info: None,
                        token_usage: None,
                        turn_token_usage: None,
                        context_breakdown: None,
                        images: None,
                    },
                }));
            }

            if let Some(ready_tx) = ready_tx.take() {
                let _ = ready_tx.send(Err(tycode_startup_exit_error(&last_stderr_line)));
            }
        });

        await_tycode_startup(ready_rx, &shutdown_tx, "resume", &startup_status).await?;

        Ok((
            Self {
                input_tx,
                interrupt_tx,
                shutdown_tx,
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
            backend_fork_unsupported_message(BackendKind::Tycode),
        ))
    }

    async fn list_sessions() -> Result<Vec<BackendSession>, String> {
        list_tycode_sessions()
    }

    fn session_id(&self) -> SessionId {
        self.session_id
            .lock()
            .expect("tycode session_id mutex poisoned")
            .clone()
            .expect("tycode session_id not initialized")
    }

    async fn send(&self, input: AgentInput) -> bool {
        self.input_tx.send(input).is_ok()
    }

    async fn interrupt(&self) -> bool {
        self.interrupt_tx.send(()).is_ok()
    }

    async fn shutdown(self) {
        let _ = self.shutdown_tx.send(());
    }
}

async fn write_command(stdin: &mut tokio::process::ChildStdin, command: &Value) -> bool {
    let line = match serde_json::to_string(command) {
        Ok(s) => s,
        Err(err) => {
            tracing::error!("Failed to serialize tycode command: {err}");
            return false;
        }
    };

    if let Err(err) = stdin.write_all(line.as_bytes()).await {
        tracing::error!("Failed to write to tycode-subprocess stdin: {err}");
        return false;
    }
    if let Err(err) = stdin.write_all(b"\n").await {
        tracing::error!("Failed to write newline to tycode-subprocess stdin: {err}");
        return false;
    }
    if let Err(err) = stdin.flush().await {
        tracing::error!("Failed to flush tycode-subprocess stdin: {err}");
        return false;
    }
    true
}

async fn write_cancel(stdin: &mut tokio::process::ChildStdin) -> bool {
    if let Err(err) = stdin.write_all(b"CANCEL\n").await {
        tracing::error!("Failed to write cancel to tycode-subprocess stdin: {err}");
        return false;
    }
    if let Err(err) = stdin.flush().await {
        tracing::error!("Failed to flush tycode-subprocess cancel: {err}");
        return false;
    }
    true
}

fn tycode_sessions_dir() -> Result<PathBuf, String> {
    #[cfg(test)]
    if let Some(path) = TEST_TYCODE_SESSIONS_DIR
        .lock()
        .expect("test Tycode sessions dir mutex poisoned")
        .clone()
    {
        return Ok(path);
    }

    Ok(crate::paths::home_dir()?.join(".tycode").join("sessions"))
}

fn build_tycode_mcp_servers_json(startup_mcp_servers: &[StartupMcpServer]) -> Option<String> {
    if startup_mcp_servers.is_empty() {
        return None;
    }

    let mut servers = serde_json::Map::new();
    for server in startup_mcp_servers {
        let name = server.name.trim();
        if name.is_empty() {
            continue;
        }
        let config = match &server.transport {
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
                config.insert("url".to_string(), Value::String(trimmed_url.to_string()));
                if !headers.is_empty() {
                    config.insert(
                        "headers".to_string(),
                        serde_json::to_value(headers)
                            .expect("HashMap<String, String> is always serializable"),
                    );
                }
                if let Some(env_var) = bearer_token_env_var
                    .as_ref()
                    .map(|raw| raw.trim())
                    .filter(|raw| !raw.is_empty())
                {
                    config.insert(
                        "bearer_token_env_var".to_string(),
                        Value::String(env_var.to_string()),
                    );
                }
                Value::Object(config)
            }
            StartupMcpTransport::Stdio { command, args, env } => {
                let trimmed_command = command.trim();
                if trimmed_command.is_empty() {
                    continue;
                }
                serde_json::json!({
                    "command": trimmed_command,
                    "args": args,
                    "env": env,
                })
            }
        };
        servers.insert(name.to_string(), config);
    }

    if servers.is_empty() {
        return None;
    }

    Some(Value::Object(servers).to_string())
}

fn spawn_tycode_stderr_logger(
    stderr: tokio::process::ChildStderr,
) -> Arc<std::sync::Mutex<Option<String>>> {
    let last_stderr_line = Arc::new(std::sync::Mutex::new(None));
    let sink = Arc::clone(&last_stderr_line);
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            tracing::warn!("tycode-subprocess stderr: {trimmed}");
            *sink.lock().expect("tycode stderr mutex poisoned") = Some(trimmed.to_string());
        }
    });
    last_stderr_line
}

fn tycode_startup_exit_error(last_stderr_line: &Arc<std::sync::Mutex<Option<String>>>) -> String {
    match last_stderr_line
        .lock()
        .expect("tycode stderr mutex poisoned")
        .clone()
    {
        Some(stderr) => {
            format!("Tycode process exited before reporting a session_id: {stderr}")
        }
        None => "Tycode process exited before reporting a session_id".to_string(),
    }
}

fn tycode_session_started(value: &Value) -> Option<SessionId> {
    if value.get("kind").and_then(Value::as_str) != Some("SessionStarted") {
        return None;
    }

    value
        .get("data")
        .and_then(|data| data.get("session_id"))
        .and_then(Value::as_str)
        .map(|session_id| SessionId(session_id.to_string()))
}

fn list_tycode_sessions() -> Result<Vec<BackendSession>, String> {
    let sessions_dir = tycode_sessions_dir()?;
    let entries = match fs::read_dir(&sessions_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => {
            return Err(format!(
                "Failed to read Tycode sessions directory {}: {err}",
                sessions_dir.display()
            ));
        }
    };

    let mut sessions = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                tracing::warn!("Skipping unreadable Tycode session entry: {err}");
                continue;
            }
        };
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }

        let json = match fs::read_to_string(&path) {
            Ok(json) => json,
            Err(err) => {
                tracing::warn!("Skipping unreadable Tycode session {:?}: {err}", path);
                continue;
            }
        };
        let value: Value = match serde_json::from_str(&json) {
            Ok(value) => value,
            Err(err) => {
                tracing::warn!("Skipping unparseable Tycode session {:?}: {err}", path);
                continue;
            }
        };

        let Some(id) = value.get("id").and_then(Value::as_str).map(str::to_string) else {
            continue;
        };
        let created_at_ms = value.get("created_at").and_then(Value::as_u64);
        let updated_at_ms = value.get("last_modified").and_then(Value::as_u64);
        let title = extract_tycode_title(&value);

        sessions.push(BackendSession {
            id: SessionId(id),
            backend_kind: BackendKind::Tycode,
            workspace_roots: Vec::new(),
            title,
            token_count: None,
            created_at_ms,
            updated_at_ms,
            resumable: true,
        });
    }

    sessions.sort_by_key(|session| std::cmp::Reverse(session.updated_at_ms));
    Ok(sessions)
}

fn extract_tycode_title(value: &Value) -> Option<String> {
    let messages = value.get("messages")?.as_array()?;
    for message in messages {
        if message.get("role").and_then(Value::as_str) != Some("User") {
            continue;
        }
        if let Some(text) = message
            .get("content")
            .and_then(|content| content.get("blocks"))
            .and_then(Value::as_array)
            .and_then(|blocks| blocks.first())
            .and_then(|block| block.get("text"))
            .and_then(Value::as_str)
        {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.chars().take(80).collect());
            }
        }
    }
    None
}

fn is_tycode_sessions_list(value: &Value) -> bool {
    value.get("kind").and_then(Value::as_str) == Some("SessionsList")
}

struct TycodeResumeReplayBarrier {
    session_id: String,
    replay_started: bool,
    replay_events_remaining: usize,
}

impl TycodeResumeReplayBarrier {
    fn new(session_id: String, replay_events_remaining: usize) -> Self {
        Self {
            session_id,
            replay_started: false,
            replay_events_remaining,
        }
    }

    fn observe(&mut self, value: &Value) -> bool {
        if !self.replay_started {
            if is_tycode_session_started(value, &self.session_id) {
                self.replay_started = true;
                self.replay_events_remaining = self.replay_events_remaining.saturating_sub(1);
            }
            return false;
        }
        if self.replay_events_remaining > 0 {
            self.replay_events_remaining -= 1;
            return false;
        }
        is_tycode_sessions_list(value)
    }
}

fn is_tycode_session_started(value: &Value, session_id: &str) -> bool {
    value.get("kind").and_then(Value::as_str) == Some("SessionStarted")
        && value
            .get("data")
            .and_then(|data| data.get("session_id"))
            .and_then(Value::as_str)
            == Some(session_id)
}

fn tycode_resume_replay_event_count(session_id: &SessionId) -> Result<usize, String> {
    let path = tycode_sessions_dir()?.join(format!("{}.json", session_id.0));
    let json = fs::read_to_string(&path)
        .map_err(|err| format!("failed to read Tycode session {}: {err}", path.display()))?;
    tycode_resume_replay_event_count_from_json(&json)
}

fn tycode_resume_replay_event_count_from_json(json: &str) -> Result<usize, String> {
    let value: Value = serde_json::from_str(json)
        .map_err(|err| format!("failed to parse Tycode session JSON: {err}"))?;
    let events = value
        .get("events")
        .and_then(Value::as_array)
        .ok_or_else(|| "Tycode session JSON is missing an events array".to_owned())?;
    Ok(2 + events
        .iter()
        .filter(|event| !is_tycode_replay_filtered_delta(event))
        .count())
}

fn is_tycode_replay_filtered_delta(value: &Value) -> bool {
    matches!(
        value.get("kind").and_then(Value::as_str),
        Some("StreamDelta" | "StreamReasoningDelta")
    )
}

fn map_tycode_value_to_chat_events(value: &Value) -> Vec<ChatEvent> {
    if let Ok(event) = serde_json::from_value::<ChatEvent>(value.clone()) {
        return vec![event];
    }

    let Some(kind) = value.get("kind").and_then(Value::as_str) else {
        tracing::warn!(raw = %value, "Ignoring Tycode event without kind");
        return Vec::new();
    };

    match kind {
        "Settings"
        | "ConversationCleared"
        | "SessionsList"
        | "ProfilesList"
        | "TimingUpdate"
        | "ModuleSchemas"
        | "SessionStarted"
        | "Error" => Vec::new(),
        other => {
            tracing::warn!(kind = %other, raw = %value, "Ignoring unsupported Tycode event");
            Vec::new()
        }
    }
}

fn update_tycode_stream_state(
    event: &ChatEvent,
    stream_open: &mut bool,
    accumulated_text: &mut String,
) {
    match event {
        ChatEvent::StreamStart(_) => {
            *stream_open = true;
            accumulated_text.clear();
        }
        ChatEvent::StreamDelta(StreamTextDeltaData { text, .. }) if *stream_open => {
            accumulated_text.push_str(text);
        }
        ChatEvent::StreamEnd(_) => {
            *stream_open = false;
        }
        _ => {}
    }
}

fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};

    use super::*;
    use protocol::SendMessagePayload;
    use tempfile::TempDir;

    const TEST_TYCODE_STARTUP_TIMEOUT_DURATION: Duration = Duration::from_secs(2);

    #[test]
    fn tycode_backend_config_schema_exposes_runtime_json_settings() {
        let schema = tycode_backend_config_schema();
        assert_eq!(schema.backend_kind, BackendKind::Tycode);
        let keys = schema
            .fields
            .iter()
            .map(|field| field.key.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            keys,
            vec![
                "active_provider",
                "model_quality",
                "reasoning_effort",
                "autonomy_level",
                "review_level",
                "spawn_context_mode",
            ]
        );

        let active_provider = schema
            .fields
            .iter()
            .find(|field| field.key == "active_provider")
            .expect("active_provider field");
        assert!(matches!(
            &active_provider.field_type,
            BackendConfigFieldType::Text { .. }
        ));

        let model_quality = schema
            .fields
            .iter()
            .find(|field| field.key == "model_quality")
            .expect("model_quality field");
        match &model_quality.field_type {
            BackendConfigFieldType::Select {
                options, nullable, ..
            } => {
                assert!(*nullable);
                assert_eq!(
                    options
                        .iter()
                        .map(|option| option.value.as_str())
                        .collect::<Vec<_>>(),
                    vec!["free", "low", "medium", "high", "unlimited"]
                );
            }
            other => panic!("model_quality should be Select, got {other:?}"),
        }

        let auto_selects = schema
            .fields
            .iter()
            .filter(|field| {
                matches!(
                    field.key.as_str(),
                    "autonomy_level" | "review_level" | "spawn_context_mode"
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(auto_selects.len(), 3);
        assert!(auto_selects.iter().all(|field| matches!(
            &field.field_type,
            BackendConfigFieldType::Select {
                default: None,
                nullable: true,
                ..
            }
        )));
    }

    #[test]
    fn tycode_settings_overlay_preserves_providers_and_unmanaged_keys() {
        let settings = serde_json::json!({
            "active_provider": "default",
            "providers": {
                "default": {
                    "type": "openrouter",
                    "api_key": "secret",
                    "unmanaged_provider_key": { "keep": true }
                },
                "other": {
                    "type": "codex",
                    "command": "codex"
                }
            },
            "model_quality": null,
            "reasoning_effort": null,
            "review_level": "None",
            "spawn_context_mode": "Fork",
            "disable_custom_steering": false,
            "disable_streaming": false,
            "unmanaged_top_level": { "still": "here" }
        });
        let original_providers = settings["providers"].clone();
        let mut config = BackendConfigValues::default();
        config.0.insert(
            "active_provider".to_string(),
            SessionSettingValue::String("other".to_string()),
        );
        config.0.insert(
            "model_quality".to_string(),
            SessionSettingValue::String("low".to_string()),
        );
        config.0.insert(
            "reasoning_effort".to_string(),
            SessionSettingValue::String("Max".to_string()),
        );
        config.0.insert(
            "review_level".to_string(),
            SessionSettingValue::String("Task".to_string()),
        );
        config.0.insert(
            "spawn_context_mode".to_string(),
            SessionSettingValue::String("Fresh".to_string()),
        );
        config.0.insert(
            "unknown".to_string(),
            SessionSettingValue::String("ignored".to_string()),
        );

        let overlay =
            apply_tycode_backend_config_overlay(&settings, &config).expect("overlay settings");
        assert_eq!(overlay.active_provider_change.as_deref(), Some("other"));
        assert_eq!(overlay.settings["providers"], original_providers);
        assert_eq!(
            overlay.settings["unmanaged_top_level"],
            serde_json::json!({ "still": "here" })
        );
        assert_eq!(overlay.settings["active_provider"], "other");
        assert_eq!(overlay.settings["model_quality"], "low");
        assert_eq!(overlay.settings["reasoning_effort"], "Max");
        assert_eq!(overlay.settings["review_level"], "Task");
        assert_eq!(overlay.settings["spawn_context_mode"], "Fresh");
        assert_eq!(overlay.settings["disable_custom_steering"], false);
        assert_eq!(overlay.settings["disable_streaming"], false);
        assert!(overlay.settings.get("unknown").is_none());
    }

    #[test]
    fn tycode_settings_overlay_treats_nullable_auto_as_noop() {
        let settings = serde_json::json!({
            "active_provider": "default",
            "providers": {
                "default": { "type": "mock" }
            },
            "model_quality": "high",
            "reasoning_effort": "Max",
            "autonomy_level": "fully_autonomous",
            "review_level": "Task",
            "spawn_context_mode": "Fresh"
        });
        let mut config = BackendConfigValues::default();
        for key in [
            "model_quality",
            "reasoning_effort",
            "autonomy_level",
            "review_level",
            "spawn_context_mode",
        ] {
            config.0.insert(key.to_string(), SessionSettingValue::Null);
        }

        let overlay =
            apply_tycode_backend_config_overlay(&settings, &config).expect("overlay settings");
        assert_eq!(overlay.settings, settings);
        assert_eq!(overlay.active_provider_change, None);
    }

    #[test]
    fn tycode_settings_overlay_rejects_absent_active_provider() {
        let settings = serde_json::json!({
            "active_provider": "default",
            "providers": {
                "default": { "type": "mock" }
            }
        });
        let mut config = BackendConfigValues::default();
        config.0.insert(
            "active_provider".to_string(),
            SessionSettingValue::String("missing".to_string()),
        );

        let err =
            apply_tycode_backend_config_overlay(&settings, &config).expect_err("missing provider");
        assert!(err.contains("Configured Tycode active_provider 'missing' is absent"));
        assert!(err.contains("available: default"));
    }

    #[test]
    fn tycode_settings_verification_error_redacts_provider_values() {
        let expected = serde_json::json!({
            "active_provider": "default",
            "providers": {
                "default": {
                    "type": "openrouter",
                    "api_key": "secret"
                }
            },
            "model_quality": "high"
        });
        let actual = serde_json::json!({
            "active_provider": "default",
            "providers": {
                "default": {
                    "type": "openrouter",
                    "api_key": "different-secret"
                }
            },
            "model_quality": "low"
        });

        let err = tycode_settings_verification_error(&expected, &actual);
        assert!(err.contains("mismatched managed keys: model_quality"));
        assert!(err.contains("providers changed"));
        assert!(!err.contains("secret"));
        assert!(!err.contains("different-secret"));
    }

    #[test]
    fn tycode_settings_verification_allows_unmanaged_changes() {
        let expected = serde_json::json!({
            "active_provider": "default",
            "providers": {
                "default": { "type": "mock" }
            },
            "model_quality": "high",
            "reasoning_effort": null,
            "autonomy_level": "plan_approval_required",
            "review_level": "None",
            "spawn_context_mode": "Fork",
            "unmanaged": "before"
        });
        let actual = serde_json::json!({
            "active_provider": "default",
            "providers": {
                "default": { "type": "mock" }
            },
            "model_quality": "high",
            "reasoning_effort": null,
            "autonomy_level": "plan_approval_required",
            "review_level": "None",
            "spawn_context_mode": "Fork",
            "unmanaged": "after"
        });

        verify_tycode_settings_overlay(&expected, &actual)
            .expect("unmanaged settings changes should not fail verification");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fake_tycode_spawn_applies_settings_before_user_input() {
        let dir = TempDir::new().expect("tempdir");
        let settings = serde_json::json!({
            "active_provider": "default",
            "providers": {
                "default": { "type": "mock" },
                "other": { "type": "openrouter", "api_key": "secret" }
            },
            "model_quality": null,
            "reasoning_effort": null,
            "autonomy_level": "plan_approval_required",
            "review_level": "None",
            "spawn_context_mode": "Fork",
            "disable_custom_steering": false,
            "disable_streaming": false
        });
        let fake = write_fake_tycode_subprocess(dir.path(), &settings);
        let log = dir.path().join("commands.jsonl");
        let _guard = TestTycodeSubprocessGuard::set(fake);

        let mut config = BackendSpawnConfig::default();
        config.backend_config.0.insert(
            "active_provider".to_string(),
            SessionSettingValue::String("other".to_string()),
        );
        config.backend_config.0.insert(
            "model_quality".to_string(),
            SessionSettingValue::String("high".to_string()),
        );
        config.backend_config.0.insert(
            "spawn_context_mode".to_string(),
            SessionSettingValue::String("Fresh".to_string()),
        );

        let (backend, mut events) =
            TycodeBackend::spawn(Vec::new(), config, payload("hello Tycode"))
                .await
                .expect("spawn fake Tycode");
        wait_for_fake_done(&mut events).await;
        backend.shutdown().await;

        let commands = read_fake_commands(&log);
        assert_eq!(commands.len(), 5, "commands: {commands:#?}");
        assert_eq!(commands[0], Value::String("GetSettings".to_string()));
        assert_eq!(commands[2], Value::String("GetSettings".to_string()));
        assert_eq!(
            commands[3],
            serde_json::json!({ "ChangeProvider": "other" })
        );
        assert_eq!(
            commands[4],
            serde_json::json!({ "UserInput": "hello Tycode" })
        );

        let save = commands[1]
            .get("SaveSettings")
            .expect("SaveSettings command");
        assert_eq!(save["persist"], false);
        assert_eq!(save["settings"]["active_provider"], "other");
        assert_eq!(save["settings"]["model_quality"], "high");
        assert_eq!(save["settings"]["spawn_context_mode"], "Fresh");
        assert_eq!(save["settings"]["disable_streaming"], false);
        assert_eq!(save["settings"]["providers"], settings["providers"]);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fake_tycode_spawn_sends_change_provider_for_explicit_same_provider() {
        let dir = TempDir::new().expect("tempdir");
        let settings = serde_json::json!({
            "active_provider": "default",
            "providers": {
                "default": { "type": "mock" }
            },
            "model_quality": null,
            "reasoning_effort": null,
            "autonomy_level": "plan_approval_required",
            "review_level": "None",
            "spawn_context_mode": "Fork"
        });
        let fake = write_fake_tycode_subprocess(dir.path(), &settings);
        let log = dir.path().join("commands.jsonl");
        let _guard = TestTycodeSubprocessGuard::set(fake);

        let mut config = BackendSpawnConfig::default();
        config.backend_config.0.insert(
            "active_provider".to_string(),
            SessionSettingValue::String("default".to_string()),
        );

        let (backend, mut events) =
            TycodeBackend::spawn(Vec::new(), config, payload("hello Tycode"))
                .await
                .expect("spawn fake Tycode");
        wait_for_fake_done(&mut events).await;
        backend.shutdown().await;

        let commands = read_fake_commands(&log);
        assert_eq!(commands.len(), 5, "commands: {commands:#?}");
        assert_eq!(commands[0], Value::String("GetSettings".to_string()));
        assert_eq!(commands[2], Value::String("GetSettings".to_string()));
        assert_eq!(
            commands[3],
            serde_json::json!({ "ChangeProvider": "default" })
        );
        assert_eq!(
            commands[4],
            serde_json::json!({ "UserInput": "hello Tycode" })
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fake_tycode_spawn_fails_before_prompt_for_invalid_provider() {
        let dir = TempDir::new().expect("tempdir");
        let settings = serde_json::json!({
            "active_provider": "default",
            "providers": {
                "default": { "type": "mock" }
            }
        });
        let fake = write_fake_tycode_subprocess(dir.path(), &settings);
        let log = dir.path().join("commands.jsonl");
        let _guard = TestTycodeSubprocessGuard::set(fake);

        let mut config = BackendSpawnConfig::default();
        config.backend_config.0.insert(
            "active_provider".to_string(),
            SessionSettingValue::String("missing".to_string()),
        );

        let err = match TycodeBackend::spawn(Vec::new(), config, payload("must not send")).await {
            Ok(_) => panic!("invalid provider should fail startup"),
            Err(err) => err,
        };
        assert!(err.contains("active_provider 'missing' is absent"));

        let commands = read_fake_commands(&log);
        assert_eq!(commands, vec![Value::String("GetSettings".to_string())]);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fake_tycode_spawn_times_out_waiting_for_settings() {
        let dir = TempDir::new().expect("tempdir");
        let fake = write_fake_tycode_settings_stall_subprocess(dir.path());
        let log = dir.path().join("commands.jsonl");
        let _guard = TestTycodeSubprocessGuard::set_with_options(
            fake,
            None,
            Some(TEST_TYCODE_STARTUP_TIMEOUT_DURATION),
        );

        let mut config = BackendSpawnConfig::default();
        config.backend_config.0.insert(
            "active_provider".to_string(),
            SessionSettingValue::String("default".to_string()),
        );

        let spawn_handle = tokio::spawn(async move {
            TycodeBackend::spawn(Vec::new(), config, payload("must not send")).await
        });

        let commands = read_fake_commands_eventually(&log, 1).await;
        assert_eq!(commands, vec![Value::String("GetSettings".to_string())]);

        let err = match spawn_handle.await.expect("fake Tycode spawn task panicked") {
            Ok(_) => panic!("settings stall should fail startup"),
            Err(err) => err,
        };
        assert!(
            err.contains("Timed out after 2s"),
            "unexpected error: {err}"
        );
        assert!(
            err.contains("Tycode spawn startup/settings handshake"),
            "unexpected error: {err}"
        );
        assert!(
            err.contains("waiting for Settings after GetSettings"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fake_tycode_resume_times_out_waiting_for_settings() {
        let dir = TempDir::new().expect("tempdir");
        let fake = write_fake_tycode_settings_stall_subprocess(dir.path());
        let log = dir.path().join("commands.jsonl");
        let sessions_dir = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions_dir).expect("create fake sessions dir");
        std::fs::write(
            sessions_dir.join("resume-session.json"),
            serde_json::json!({
                "id": "resume-session",
                "events": []
            })
            .to_string(),
        )
        .expect("write fake Tycode resume session");
        let _guard = TestTycodeSubprocessGuard::set_with_options(
            fake,
            Some(sessions_dir),
            Some(TEST_TYCODE_STARTUP_TIMEOUT_DURATION),
        );

        let mut config = BackendSpawnConfig::default();
        config.backend_config.0.insert(
            "active_provider".to_string(),
            SessionSettingValue::String("default".to_string()),
        );

        let resume_handle = tokio::spawn(async move {
            TycodeBackend::resume(Vec::new(), config, SessionId("resume-session".to_string())).await
        });

        let commands = read_fake_commands_eventually(&log, 1).await;
        assert_eq!(commands, vec![Value::String("GetSettings".to_string())]);

        let err = match resume_handle
            .await
            .expect("fake Tycode resume task panicked")
        {
            Ok(_) => panic!("settings stall should fail resume startup"),
            Err(err) => err,
        };
        assert!(err.contains("Timed out after 2s"));
        assert!(err.contains("Tycode resume startup/settings handshake"));
        assert!(err.contains("waiting for Settings after GetSettings"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fake_tycode_spawn_times_out_waiting_for_change_provider_ack() {
        let dir = TempDir::new().expect("tempdir");
        let settings = serde_json::json!({
            "active_provider": "default",
            "providers": {
                "default": { "type": "mock" },
                "other": { "type": "mock" }
            },
            "model_quality": null,
            "reasoning_effort": null,
            "autonomy_level": "plan_approval_required",
            "review_level": "None",
            "spawn_context_mode": "Fork"
        });
        let fake = write_fake_tycode_provider_ack_stall_subprocess(dir.path(), &settings);
        let log = dir.path().join("commands.jsonl");
        let _guard = TestTycodeSubprocessGuard::set_with_options(
            fake,
            None,
            Some(TEST_TYCODE_STARTUP_TIMEOUT_DURATION),
        );

        let mut config = BackendSpawnConfig::default();
        config.backend_config.0.insert(
            "active_provider".to_string(),
            SessionSettingValue::String("other".to_string()),
        );

        let spawn_handle = tokio::spawn(async move {
            TycodeBackend::spawn(Vec::new(), config, payload("must not send")).await
        });

        let commands = read_fake_commands_eventually(&log, 4).await;
        assert_eq!(commands.len(), 4, "commands: {commands:#?}");
        assert_eq!(commands[0], Value::String("GetSettings".to_string()));
        assert_eq!(commands[2], Value::String("GetSettings".to_string()));
        assert_eq!(
            commands[3],
            serde_json::json!({ "ChangeProvider": "other" })
        );

        let err = match spawn_handle.await.expect("fake Tycode spawn task panicked") {
            Ok(_) => panic!("provider ack stall should fail startup"),
            Err(err) => err,
        };
        assert!(err.contains("Timed out after 2s"));
        assert!(err.contains("waiting for ChangeProvider acknowledgement"));
    }

    #[test]
    fn build_tycode_mcp_servers_json_supports_http_servers() {
        let json = build_tycode_mcp_servers_json(&[StartupMcpServer {
            name: "tyde-debug".to_string(),
            transport: StartupMcpTransport::Http {
                url: "http://127.0.0.1:4123/mcp".to_string(),
                headers: HashMap::from([(
                    "x-tyde-debug-repo-root".to_string(),
                    "/tmp/project".to_string(),
                )]),
                bearer_token_env_var: None,
            },
        }])
        .expect("HTTP MCP config should serialize");
        let value: Value = serde_json::from_str(&json).expect("parse JSON");
        assert_eq!(
            value["tyde-debug"]["url"],
            Value::String("http://127.0.0.1:4123/mcp".to_string())
        );
        assert_eq!(
            value["tyde-debug"]["headers"]["x-tyde-debug-repo-root"],
            Value::String("/tmp/project".to_string())
        );
    }

    struct TestTycodeSubprocessGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        previous_bin: Option<String>,
        previous_sessions_dir: Option<PathBuf>,
        previous_timeout: Option<Duration>,
    }

    static TEST_TYCODE_SUBPROCESS_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    impl TestTycodeSubprocessGuard {
        fn set(path: String) -> Self {
            Self::set_with_options(path, None, None)
        }

        fn set_with_options(
            path: String,
            sessions_dir: Option<PathBuf>,
            startup_timeout: Option<Duration>,
        ) -> Self {
            let _ = crate::process_env::resolved_child_process_path();
            let lock = TEST_TYCODE_SUBPROCESS_MUTEX
                .lock()
                .expect("test Tycode subprocess mutex poisoned");
            let mut configured = TEST_TYCODE_SUBPROCESS_BIN
                .lock()
                .expect("test Tycode subprocess bin mutex poisoned");
            let previous_bin = configured.replace(path);
            drop(configured);
            let mut configured_sessions_dir = TEST_TYCODE_SESSIONS_DIR
                .lock()
                .expect("test Tycode sessions dir mutex poisoned");
            let previous_sessions_dir =
                std::mem::replace(&mut *configured_sessions_dir, sessions_dir);
            drop(configured_sessions_dir);
            let mut configured_timeout = TEST_TYCODE_STARTUP_TIMEOUT
                .lock()
                .expect("test Tycode startup timeout mutex poisoned");
            let previous_timeout = std::mem::replace(&mut *configured_timeout, startup_timeout);
            drop(configured_timeout);
            Self {
                _lock: lock,
                previous_bin,
                previous_sessions_dir,
                previous_timeout,
            }
        }
    }

    impl Drop for TestTycodeSubprocessGuard {
        fn drop(&mut self) {
            *TEST_TYCODE_SUBPROCESS_BIN
                .lock()
                .expect("test Tycode subprocess bin mutex poisoned") = self.previous_bin.take();
            *TEST_TYCODE_SESSIONS_DIR
                .lock()
                .expect("test Tycode sessions dir mutex poisoned") =
                self.previous_sessions_dir.take();
            *TEST_TYCODE_STARTUP_TIMEOUT
                .lock()
                .expect("test Tycode startup timeout mutex poisoned") =
                self.previous_timeout.take();
        }
    }

    fn write_fake_tycode_subprocess(dir: &Path, settings: &Value) -> String {
        let script = dir.join("fake_tycode_subprocess.py");
        let log = dir.join("commands.jsonl");
        let settings_literal =
            serde_json::to_string(&settings.to_string()).expect("settings literal");
        let log_literal = serde_json::to_string(&log.to_string_lossy()).expect("log literal");
        let body = r#"#!/usr/bin/env python3
import json
import sys

settings = json.loads(__SETTINGS__)
log_path = __LOG__

def emit(value):
    print(json.dumps(value, separators=(",", ":")), flush=True)

def message(sender, content):
    return {
        "kind": "MessageAdded",
        "data": {
            "timestamp": 1,
            "sender": sender,
            "content": content,
            "reasoning": None,
            "tool_calls": [],
            "model_info": None,
            "token_usage": None,
            "context_breakdown": None,
            "images": [],
        },
    }

emit({"kind": "SessionStarted", "data": {"session_id": "fake-session"}})

for raw_line in sys.stdin:
    line = raw_line.strip()
    if not line:
        continue
    with open(log_path, "a", encoding="utf-8") as log:
        log.write(line + "\n")
    command = json.loads(line)
    if command == "GetSettings":
        emit({"kind": "Settings", "data": settings})
    elif isinstance(command, dict) and "SaveSettings" in command:
        settings = command["SaveSettings"]["settings"]
    elif isinstance(command, dict) and "ChangeProvider" in command:
        emit(message("System", f"Switched to provider: {command['ChangeProvider']}"))
    elif isinstance(command, dict) and "UserInput" in command:
        emit(message({"Assistant": {"agent": "tycode"}}, "fake done"))
"#
        .replace("__SETTINGS__", &settings_literal)
        .replace("__LOG__", &log_literal);
        std::fs::write(&script, body).expect("write fake Tycode script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&script)
                .expect("stat fake Tycode script")
                .permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&script, permissions).expect("chmod fake Tycode script");
        }
        script.to_string_lossy().to_string()
    }

    fn write_fake_tycode_settings_stall_subprocess(dir: &Path) -> String {
        let script = dir.join("fake_tycode_settings_stall.sh");
        let log = dir.join("commands.jsonl");
        let log_literal = serde_json::to_string(&log.to_string_lossy()).expect("log literal");
        let body = r#"#!/bin/sh
LOG_PATH=__LOG__
printf '%s\n' '{"kind":"SessionStarted","data":{"session_id":"fake-session"}}'
while IFS= read -r line; do
  [ -n "$line" ] || continue
  printf '%s\n' "$line" >> "$LOG_PATH"
done
"#
        .replace("__LOG__", &log_literal);
        std::fs::write(&script, body).expect("write fake Tycode stall script");
        make_executable(&script);
        script.to_string_lossy().to_string()
    }

    fn write_fake_tycode_provider_ack_stall_subprocess(dir: &Path, settings: &Value) -> String {
        let script = dir.join("fake_tycode_provider_ack_stall.py");
        let log = dir.join("commands.jsonl");
        let settings_literal =
            serde_json::to_string(&settings.to_string()).expect("settings literal");
        let log_literal = serde_json::to_string(&log.to_string_lossy()).expect("log literal");
        let body = r#"#!/usr/bin/env python3
import json
import sys

settings = json.loads(__SETTINGS__)
log_path = __LOG__

def emit(value):
    print(json.dumps(value, separators=(",", ":")), flush=True)

emit({"kind": "SessionStarted", "data": {"session_id": "fake-session"}})

for raw_line in sys.stdin:
    line = raw_line.strip()
    if not line:
        continue
    with open(log_path, "a", encoding="utf-8") as log:
        log.write(line + "\n")
    command = json.loads(line)
    if command == "GetSettings":
        emit({"kind": "Settings", "data": settings})
    elif isinstance(command, dict) and "SaveSettings" in command:
        settings = command["SaveSettings"]["settings"]
"#
        .replace("__SETTINGS__", &settings_literal)
        .replace("__LOG__", &log_literal);
        std::fs::write(&script, body).expect("write fake Tycode provider ack stall script");
        make_executable(&script);
        script.to_string_lossy().to_string()
    }

    fn make_executable(path: &Path) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(path)
                .expect("stat fake Tycode script")
                .permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(path, permissions).expect("chmod fake Tycode script");
        }
    }

    fn read_fake_commands(log: &Path) -> Vec<Value> {
        let body = std::fs::read_to_string(log).expect("read fake Tycode command log");
        body.lines()
            .map(|line| serde_json::from_str(line).expect("parse fake Tycode command"))
            .collect()
    }

    async fn read_fake_commands_eventually(log: &Path, minimum_len: usize) -> Vec<Value> {
        for _ in 0..300 {
            if let Ok(body) = std::fs::read_to_string(log) {
                let commands = body
                    .lines()
                    .map(|line| serde_json::from_str(line).expect("parse fake Tycode command"))
                    .collect::<Vec<_>>();
                if commands.len() >= minimum_len {
                    return commands;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        read_fake_commands(log)
    }

    async fn wait_for_fake_done(events: &mut EventStream) {
        let event = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                match events.recv().await {
                    Some(ChatEvent::MessageAdded(message)) if message.content == "fake done" => {
                        return;
                    }
                    Some(_) => {}
                    None => panic!("fake Tycode event stream ended before fake done"),
                }
            }
        })
        .await;
        assert!(event.is_ok(), "timed out waiting for fake Tycode response");
    }

    fn payload(message: &str) -> SendMessagePayload {
        SendMessagePayload {
            message: message.to_string(),
            images: None,
            origin: None,
            tool_response: None,
        }
    }

    #[test]
    fn build_tycode_mcp_servers_json_supports_stdio_servers() {
        let json = build_tycode_mcp_servers_json(&[StartupMcpServer {
            name: "context7".to_string(),
            transport: StartupMcpTransport::Stdio {
                command: "npx".to_string(),
                args: vec!["@upstash/context7-mcp@latest".to_string()],
                env: HashMap::from([("FOO".to_string(), "bar".to_string())]),
            },
        }])
        .expect("stdio MCP config should serialize");
        let value: Value = serde_json::from_str(&json).expect("parse JSON");
        assert_eq!(
            value["context7"]["command"],
            Value::String("npx".to_string())
        );
        assert_eq!(
            value["context7"]["args"],
            Value::Array(vec![Value::String(
                "@upstash/context7-mcp@latest".to_string()
            )])
        );
        assert_eq!(
            value["context7"]["env"]["FOO"],
            Value::String("bar".to_string())
        );
    }

    #[test]
    fn tycode_read_only_access_mode_uses_read_only_agent_tools() {
        let agent_json = tycode_read_only_agent_json(&BackendSpawnConfig {
            resolved_spawn_config: crate::agent::customization::ResolvedSpawnConfig {
                access_mode: BackendAccessMode::ReadOnly,
                ..Default::default()
            },
            ..Default::default()
        })
        .expect("read-only agent json");
        let value: Value = serde_json::from_str(&agent_json).expect("valid agent json");
        let tools = value
            .get("tools")
            .and_then(Value::as_array)
            .expect("tools")
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>();

        assert!(tools.contains(&"set_tracked_files"));
        assert!(
            tools.contains(&"run_build_test"),
            "read-only is advisory, so build/test commands must still be available"
        );
        assert!(!tools.contains(&"write_file"));
        assert!(!tools.contains(&"modify_file"));
    }

    #[test]
    fn map_tycode_value_to_chat_events_passes_through_assistant_message_added() {
        let value = serde_json::json!({
            "kind": "MessageAdded",
            "data": {
                "timestamp": 1776827246365_u64,
                "sender": {
                    "Assistant": {
                        "agent": "tycode"
                    }
                },
                "content": "hello from tycode",
                "reasoning": null,
                "tool_calls": [],
                "model_info": null,
                "token_usage": null,
                "context_breakdown": null,
                "images": []
            }
        });

        let events = map_tycode_value_to_chat_events(&value);
        assert_eq!(
            events.len(),
            1,
            "assistant message should stay a single event"
        );

        match &events[0] {
            ChatEvent::MessageAdded(message) => {
                assert_eq!(message.timestamp, 1776827246365_u64);
                assert_eq!(message.content, "hello from tycode");
                match &message.sender {
                    MessageSender::Assistant { agent } => assert_eq!(agent, "tycode"),
                    other => panic!("expected assistant sender, got {other:?}"),
                }
            }
            other => panic!("expected MessageAdded pass-through, got {other:?}"),
        }
    }

    #[test]
    fn map_tycode_value_to_chat_events_passes_through_stream_start() {
        let value = serde_json::json!({
            "kind": "StreamStart",
            "data": {
                "message_id": "msg-1776827246365",
                "agent": "tycode",
                "model": "ClaudeSonnet46"
            }
        });

        let events = map_tycode_value_to_chat_events(&value);
        assert_eq!(events.len(), 1);

        match &events[0] {
            ChatEvent::StreamStart(data) => {
                assert_eq!(data.message_id.as_deref(), Some("msg-1776827246365"));
                assert_eq!(data.agent, "tycode");
                assert_eq!(data.model.as_deref(), Some("ClaudeSonnet46"));
            }
            other => panic!("expected StreamStart pass-through, got {other:?}"),
        }
    }

    #[test]
    fn map_tycode_value_to_chat_events_ignores_session_started() {
        let value = serde_json::json!({
            "kind": "SessionStarted",
            "data": {
                "session_id": "session-123"
            }
        });

        let events = map_tycode_value_to_chat_events(&value);
        assert!(
            events.is_empty(),
            "SessionStarted should stay out of chat streams"
        );
    }

    #[test]
    fn resume_replay_barrier_ignores_historical_sessions_list_until_replay_count_exhausted() {
        let mut barrier = TycodeResumeReplayBarrier::new("session-1".to_owned(), 5);
        let pre_resume_warning = serde_json::json!({
            "kind": "MessageAdded",
            "data": {
                "timestamp": 1_u64,
                "sender": {
                    "Error": {}
                },
                "content": "startup warning before resume",
                "reasoning": null,
                "tool_calls": [],
                "model_info": null,
                "token_usage": null,
                "context_breakdown": null,
                "images": []
            }
        });
        let session_started = serde_json::json!({
            "kind": "SessionStarted",
            "data": { "session_id": "session-1" }
        });
        let conversation_cleared = serde_json::json!({ "kind": "ConversationCleared" });
        let historical_sessions_list = serde_json::json!({
            "kind": "SessionsList",
            "data": { "sessions": [] }
        });
        let historical_message = serde_json::json!({
            "kind": "MessageAdded",
            "data": {
                "timestamp": 1_u64,
                "sender": {
                    "Assistant": {
                        "agent": "tycode"
                    }
                },
                "content": "still replayed after historical SessionsList",
                "reasoning": null,
                "tool_calls": [],
                "model_info": null,
                "token_usage": null,
                "context_breakdown": null,
                "images": []
            }
        });
        let historical_final_sessions_list = serde_json::json!({
            "kind": "SessionsList",
            "data": { "sessions": [] }
        });
        let genuine_sentinel = serde_json::json!({
            "kind": "SessionsList",
            "data": { "sessions": [] }
        });

        assert!(
            !barrier.observe(&pre_resume_warning),
            "pre-resume startup output must not consume replay count or complete the barrier"
        );
        for event in [
            &session_started,
            &conversation_cleared,
            &historical_sessions_list,
            &historical_message,
            &historical_final_sessions_list,
        ] {
            assert!(
                !barrier.observe(event),
                "historical replay event must not complete the barrier: {event}"
            );
        }
        assert!(
            barrier.observe(&genuine_sentinel),
            "the post-resume ListSessions response should complete the barrier"
        );
    }

    #[test]
    fn resume_replay_event_count_includes_historical_sessions_list_and_skips_deltas() {
        let session = serde_json::json!({
            "id": "session-1",
            "events": [
                { "kind": "SessionsList", "data": { "sessions": [] } },
                { "kind": "StreamDelta", "data": { "message_id": "m1", "text": "skip" } },
                { "kind": "StreamReasoningDelta", "data": { "message_id": "m1", "text": "skip" } },
                { "kind": "MessageAdded", "data": { "content": "keep" } }
            ]
        });
        let count = tycode_resume_replay_event_count_from_json(&session.to_string())
            .expect("session replay count should parse");
        assert_eq!(
            count, 4,
            "SessionStarted and ConversationCleared plus non-delta persisted events"
        );
    }
}
