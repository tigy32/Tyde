use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use command_group::AsyncCommandGroup;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

use protocol::tycode_config::{
    TYCODE_NATIVE_SETTINGS_VERSION, TycodeNativeSettingsDoc, TycodeProfileAction,
    TycodeProfileSettings,
};
use protocol::{
    AgentInput, BackendAccessMode, BackendConfigSnapshotStatus, BackendConfigValues, BackendKind,
    BackendNativeSettingsAdvisory, BackendNativeSettingsGroup, BackendNativeSettingsSnapshot,
    ChatEvent, ChatMessage, ChatMessageId, MessageMetadataUpdateData, MessageSender, ModelInfo,
    OrchestrationEvent, ReasoningData, SelectOption, SessionId, SessionSettingField,
    SessionSettingFieldType, SessionSettingValue, SessionSettingsSchema, SessionSettingsValues,
    StreamEndData, StreamTextDeltaData,
};

use super::{
    Backend, BackendSession, BackendSpawnConfig, BackendStartupError, EventStream,
    StartupMcpServer, StartupMcpTransport, apply_session_settings_update,
    backend_fork_unsupported_message, render_combined_spawn_instructions,
    setup::{TYCODE_VERSION, ensure_tycode_command_compatible, resolve_tycode_binary_path},
};
use crate::backend::tycode_config;
use crate::process_env;

async fn subprocess_bin() -> Result<String, String> {
    #[cfg(test)]
    if let Some(path) = TEST_TYCODE_SUBPROCESS_BIN
        .lock()
        .expect("test Tycode subprocess bin mutex poisoned")
        .clone()
    {
        return Ok(path);
    }

    let path =
        resolve_tycode_binary_path().ok_or_else(|| "tycode-subprocess not found".to_string())?;
    ensure_tycode_command_compatible(&path).await
}

#[cfg(test)]
static TEST_TYCODE_SUBPROCESS_BIN: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
#[cfg(test)]
static TEST_TYCODE_SESSIONS_DIR: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);
#[cfg(test)]
static TEST_TYCODE_STARTUP_TIMEOUT: std::sync::Mutex<Option<Duration>> =
    std::sync::Mutex::new(None);
#[cfg(test)]
static TEST_TYCODE_SET_ROOT_AGENT_SUPPORTED: std::sync::Mutex<Option<bool>> =
    std::sync::Mutex::new(None);
#[cfg(test)]
static TEST_TYCODE_HOME_DIR: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);
#[cfg(test)]
static TEST_TYCODE_STARTUP_PROCESS_OBSERVER: std::sync::Mutex<
    Option<TestTycodeStartupProcessObserver>,
> = std::sync::Mutex::new(None);

#[cfg(test)]
struct TestTycodeStartupProcessObserver {
    spawned: Option<tokio::sync::oneshot::Sender<u32>>,
    reaped: Option<tokio::sync::oneshot::Sender<()>>,
}

#[cfg(test)]
fn install_tycode_startup_process_observer() -> (
    tokio::sync::oneshot::Receiver<u32>,
    tokio::sync::oneshot::Receiver<()>,
) {
    let (spawned_tx, spawned_rx) = tokio::sync::oneshot::channel();
    let (reaped_tx, reaped_rx) = tokio::sync::oneshot::channel();
    *TEST_TYCODE_STARTUP_PROCESS_OBSERVER
        .lock()
        .expect("test Tycode startup process observer mutex poisoned") =
        Some(TestTycodeStartupProcessObserver {
            spawned: Some(spawned_tx),
            reaped: Some(reaped_tx),
        });
    (spawned_rx, reaped_rx)
}

#[cfg(test)]
fn observe_tycode_startup_process_spawned(pid: Option<u32>) {
    let Some(pid) = pid else {
        return;
    };
    let mut observer = TEST_TYCODE_STARTUP_PROCESS_OBSERVER
        .lock()
        .expect("test Tycode startup process observer mutex poisoned");
    if let Some(spawned) = observer
        .as_mut()
        .and_then(|observer| observer.spawned.take())
    {
        let _ = spawned.send(pid);
    }
}

#[cfg(test)]
fn observe_tycode_startup_process_reaped() {
    let observer = TEST_TYCODE_STARTUP_PROCESS_OBSERVER
        .lock()
        .expect("test Tycode startup process observer mutex poisoned")
        .take();
    if let Some(reaped) = observer.and_then(|observer| observer.reaped) {
        let _ = reaped.send(());
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TycodeCommandPurpose {
    NewSession,
    ResumeSession,
    NativeSettingsProbe,
    NativeSettingsPersist,
    LegacyConfigProbe,
    LegacyConfigPersist,
}

impl TycodeCommandPurpose {
    fn description(self) -> &'static str {
        match self {
            Self::NewSession => "new session",
            Self::ResumeSession => "resume",
            Self::NativeSettingsProbe => "native settings probe",
            Self::NativeSettingsPersist => "native settings save",
            Self::LegacyConfigProbe => "legacy configuration probe",
            Self::LegacyConfigPersist => "legacy configuration save",
        }
    }
}

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

fn raw_tycode_command(subprocess: &str, settings_path: &Path, roots_json: &str) -> Command {
    let mut command = Command::new(subprocess);
    command
        .arg("--settings-path")
        .arg(settings_path)
        .arg("--workspace-roots")
        .arg(roots_json);
    if let Some(path) = process_env::resolved_child_process_path() {
        command.env("PATH", path);
    }
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

/// The session-setting key that selects a Tycode settings profile for the
/// session's `--settings-path`.
pub(crate) const TYCODE_PROFILE_SETTING: &str = "profile";

/// The Tycode home directory (`~/.tycode`) that holds the shared settings
/// file and the `profiles/` directory.
fn tycode_home_dir() -> Result<PathBuf, String> {
    #[cfg(test)]
    if let Some(home) = TEST_TYCODE_HOME_DIR
        .lock()
        .expect("test Tycode home dir mutex poisoned")
        .clone()
    {
        return Ok(home.join(".tycode"));
    }

    Ok(crate::paths::home_dir()?.join(".tycode"))
}

/// Resolve the session's `profile` setting to the settings file the Tycode
/// subprocess launches against. An unknown or malformed profile is a visible
/// error, never a silent fall back to the shared settings file.
fn resolve_session_profile(
    settings: &SessionSettingsValues,
) -> Result<tycode_config::TycodeProfileRef, String> {
    let name = match settings.0.get(TYCODE_PROFILE_SETTING) {
        None => None,
        Some(SessionSettingValue::String(name)) => Some(name.as_str()),
        Some(other) => {
            return Err(format!(
                "Tycode profile session setting must be a string, found {other:?}"
            ));
        }
    };
    tycode_config::resolve_profile_ref_in(&tycode_home_dir()?, name)
}

/// Remove files left behind by the retired Tyde-managed settings projection.
/// The artifacts are inert — nothing reads them anymore — so a failed removal
/// must not block a launch or probe, but it is logged loudly so stale copies
/// cannot linger unnoticed.
fn cleanup_retired_projection_artifacts(home: &Path) {
    match tycode_config::cleanup_legacy_projection_artifacts_in(home) {
        Ok(removed) => {
            for path in removed {
                tracing::info!(
                    "Removed retired Tyde-managed Tycode settings projection artifact {}",
                    path.display()
                );
            }
        }
        Err(error) => tracing::warn!("{error}"),
    }
}

/// Command for a Tycode session, launched directly against the settings file
/// of the profile selected by the session's `profile` setting. There is no
/// intermediate Tyde-managed copy: what the resolved settings file says is
/// what the session runs with.
async fn tycode_session_command(
    purpose: TycodeCommandPurpose,
    config: &BackendSpawnConfig,
    roots_json: &str,
) -> Result<Command, String> {
    let resolved = resolve_session_settings(config);
    let profile = resolve_session_profile(&resolved)
        .map_err(|err| format!("Cannot start Tycode {}: {err}", purpose.description()))?;
    let home = tycode_home_dir()?;
    cleanup_retired_projection_artifacts(&home);
    let subprocess = subprocess_bin()
        .await
        .map_err(|err| format!("Cannot start Tycode {}: {err}", purpose.description()))?;
    Ok(raw_tycode_command(
        &subprocess,
        &profile.settings_path,
        roots_json,
    ))
}

/// Command for a settings probe/save conversation against one specific
/// settings file, with no workspace roots.
async fn tycode_settings_command(
    purpose: TycodeCommandPurpose,
    settings_path: &Path,
) -> Result<Command, String> {
    let subprocess = subprocess_bin()
        .await
        .map_err(|err| format!("Cannot start Tycode {}: {err}", purpose.description()))?;
    Ok(raw_tycode_command(&subprocess, settings_path, "[]"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TycodeSettingsOperationPhase {
    AwaitSessionStarted,
    AwaitSettingsSchema,
    AwaitSettingsSaved,
}

impl TycodeSettingsOperationPhase {
    fn description(self) -> &'static str {
        match self {
            Self::AwaitSessionStarted => "waiting for SessionStarted",
            Self::AwaitSettingsSchema => "waiting for SettingsSchema",
            Self::AwaitSettingsSaved => "waiting for SettingsSchema after SaveSettings",
        }
    }
}

enum TycodeSettingsRequiredResult<'a> {
    SessionStarted,
    SettingsSchema(&'a Value),
}

enum TycodeSettingsEventClassification<'a> {
    Continue,
    CollectAdvisory(BackendNativeSettingsAdvisory),
    RequiredResult(TycodeSettingsRequiredResult<'a>),
    Fatal(String),
}

fn tycode_message_added_error(value: &Value) -> Option<&str> {
    if value.get("kind").and_then(Value::as_str) != Some("MessageAdded") {
        return None;
    }
    let data = value.get("data")?;
    let error_sender = data.get("sender").and_then(Value::as_str) == Some("Error")
        || data
            .get("sender")
            .and_then(Value::as_object)
            .is_some_and(|sender| sender.contains_key("Error"));
    error_sender
        .then(|| data.get("content").and_then(Value::as_str))
        .flatten()
}

fn tycode_structured_error(value: &Value) -> Option<&str> {
    (value.get("kind").and_then(Value::as_str) == Some("Error"))
        .then(|| value.get("data").and_then(Value::as_str))
        .flatten()
}

fn tycode_settings_advisory(message: &str) -> BackendNativeSettingsAdvisory {
    let message = tycode_text_diagnostic(message);
    let lower = message.to_ascii_lowercase();
    if lower.contains("no ai provider is configured") || lower.contains("no provider is configured")
    {
        BackendNativeSettingsAdvisory::NoProviderConfigured { message }
    } else {
        BackendNativeSettingsAdvisory::BackendReported { message }
    }
}

fn classify_tycode_settings_event(
    phase: TycodeSettingsOperationPhase,
    value: &Value,
) -> TycodeSettingsEventClassification<'_> {
    if let Some(error) = tycode_structured_error(value) {
        return TycodeSettingsEventClassification::Fatal(tycode_text_diagnostic(error));
    }
    if let Some(error) = tycode_message_added_error(value) {
        return if phase == TycodeSettingsOperationPhase::AwaitSessionStarted {
            TycodeSettingsEventClassification::CollectAdvisory(tycode_settings_advisory(error))
        } else {
            TycodeSettingsEventClassification::Fatal(tycode_text_diagnostic(error))
        };
    }
    if tycode_session_started(value).is_some() {
        return if phase == TycodeSettingsOperationPhase::AwaitSessionStarted {
            TycodeSettingsEventClassification::RequiredResult(
                TycodeSettingsRequiredResult::SessionStarted,
            )
        } else {
            TycodeSettingsEventClassification::Fatal(
                "Tycode emitted an unexpected second SessionStarted event".to_string(),
            )
        };
    }
    if let Some(schema) = tycode_settings_schema_data(value) {
        return if phase == TycodeSettingsOperationPhase::AwaitSessionStarted {
            TycodeSettingsEventClassification::Fatal(
                "Tycode emitted SettingsSchema before SessionStarted".to_string(),
            )
        } else {
            TycodeSettingsEventClassification::RequiredResult(
                TycodeSettingsRequiredResult::SettingsSchema(schema),
            )
        };
    }
    TycodeSettingsEventClassification::Continue
}

fn advisory_context(advisories: &[BackendNativeSettingsAdvisory]) -> String {
    if advisories.is_empty() {
        return String::new();
    }
    let summaries = advisories
        .iter()
        .map(|advisory| match advisory {
            BackendNativeSettingsAdvisory::NoProviderConfigured { message }
            | BackendNativeSettingsAdvisory::BackendReported { message } => message.as_str(),
        })
        .collect::<Vec<_>>()
        .join("; ");
    format!("; earlier advisory: {summaries}")
}

enum TycodeSettingsOperation {
    Probe,
    Save(Value),
}

struct TycodeSettingsOperationResult {
    snapshot: BackendNativeSettingsSnapshot,
    advisories: Vec<BackendNativeSettingsAdvisory>,
}

async fn run_tycode_settings_operation(
    mut command: Command,
    purpose: TycodeCommandPurpose,
    operation: TycodeSettingsOperation,
) -> Result<TycodeSettingsOperationResult, String> {
    let mut child = command.group_spawn().map_err(|err| {
        format!(
            "Failed to spawn tycode-subprocess for {}: {err}",
            purpose.description()
        )
    })?;
    let mut stdin = child.inner().stdin.take().ok_or_else(|| {
        format!(
            "Failed to capture Tycode stdin for {}",
            purpose.description()
        )
    })?;
    let stdout = child.inner().stdout.take().ok_or_else(|| {
        format!(
            "Failed to capture Tycode stdout for {}",
            purpose.description()
        )
    })?;
    let stderr = child.inner().stderr.take().ok_or_else(|| {
        format!(
            "Failed to capture Tycode stderr for {}",
            purpose.description()
        )
    })?;
    let last_stderr_line = spawn_tycode_stderr_logger(stderr);
    let mut lines = BufReader::new(stdout).lines();
    let mut phase = TycodeSettingsOperationPhase::AwaitSessionStarted;
    let mut advisories = Vec::new();
    let deadline = tokio::time::Instant::now() + tycode_startup_timeout();

    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            let _ = child.kill().await;
            return Err(format!(
                "Timed out after {} during Tycode {}: {}{}",
                format_tycode_timeout(tycode_startup_timeout()),
                purpose.description(),
                phase.description(),
                advisory_context(&advisories)
            ));
        }
        let line = match tokio::time::timeout(deadline - now, lines.next_line()).await {
            Ok(Ok(Some(line))) => line,
            Ok(Ok(None)) => {
                let _ = child.kill().await;
                return Err(tycode_process_exit_error(
                    &last_stderr_line,
                    &format!(
                        "Tycode process exited during {}: {}{}",
                        purpose.description(),
                        phase.description(),
                        advisory_context(&advisories)
                    ),
                ));
            }
            Ok(Err(err)) => {
                let _ = child.kill().await;
                return Err(format!(
                    "Failed to read Tycode output during {}: {err}",
                    purpose.description()
                ));
            }
            Err(_) => continue,
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let value: Value = match serde_json::from_str(trimmed) {
            Ok(value) => value,
            Err(err) => {
                let _ = child.kill().await;
                return Err(format!(
                    "Malformed Tycode event during {}: {err}; event {}",
                    purpose.description(),
                    tycode_line_diagnostic(trimmed)
                ));
            }
        };
        match classify_tycode_settings_event(phase, &value) {
            TycodeSettingsEventClassification::Continue => {}
            TycodeSettingsEventClassification::CollectAdvisory(advisory) => {
                advisories.push(advisory);
            }
            TycodeSettingsEventClassification::Fatal(error) => {
                let _ = child.kill().await;
                return Err(format!(
                    "Tycode {} failed while {}: {error}{}",
                    purpose.description(),
                    phase.description(),
                    advisory_context(&advisories)
                ));
            }
            TycodeSettingsEventClassification::RequiredResult(
                TycodeSettingsRequiredResult::SessionStarted,
            ) => match &operation {
                TycodeSettingsOperation::Probe => {
                    phase = TycodeSettingsOperationPhase::AwaitSettingsSchema;
                    if !write_command(&mut stdin, &Value::String("GetSettingsSchema".to_string()))
                        .await
                    {
                        let _ = child.kill().await;
                        return Err(format!(
                            "Failed to request Tycode SettingsSchema for {}",
                            purpose.description()
                        ));
                    }
                }
                TycodeSettingsOperation::Save(settings) => {
                    phase = TycodeSettingsOperationPhase::AwaitSettingsSaved;
                    if !write_command(
                        &mut stdin,
                        &serde_json::json!({
                            "SaveSettings": {
                                "settings": settings,
                                "persist": true,
                            }
                        }),
                    )
                    .await
                        || !write_command(
                            &mut stdin,
                            &Value::String("GetSettingsSchema".to_string()),
                        )
                        .await
                    {
                        let _ = child.kill().await;
                        return Err(format!(
                            "Failed to send Tycode SaveSettings for {}",
                            purpose.description()
                        ));
                    }
                }
            },
            TycodeSettingsEventClassification::RequiredResult(
                TycodeSettingsRequiredResult::SettingsSchema(schema),
            ) => {
                let snapshot = match tycode_native_settings_snapshot_from_schema(schema) {
                    Ok(snapshot) => snapshot,
                    Err(err) => {
                        let _ = child.kill().await;
                        return Err(format!(
                            "Tycode {} returned an invalid SettingsSchema while {}: {err}{}",
                            purpose.description(),
                            phase.description(),
                            advisory_context(&advisories)
                        ));
                    }
                };
                let _ = child.kill().await;
                return Ok(TycodeSettingsOperationResult {
                    snapshot,
                    advisories,
                });
            }
        }
    }
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

fn tycode_session_settings_schema() -> SessionSettingsSchema {
    let mut fields = Vec::new();
    if tycode_set_root_agent_supported() {
        fields.push(SessionSettingField {
            key: "default_agent".to_string(),
            label: "Orchestration".to_string(),
            description: Some(
                "Controls Tycode's session root agent: None runs one agent, Auto lets Tycode \
                 delegate as needed, Pipeline runs the builder workflow, and Swarm runs the \
                 fan-out integration workflow."
                    .to_string(),
            ),
            field_type: SessionSettingFieldType::Select {
                options: vec![
                    select_option("one_shot", "None"),
                    select_option("tycode", "Auto"),
                    select_option("builder", "Pipeline"),
                    select_option("swarm", "Swarm"),
                ],
                default: Some("tycode".to_string()),
                nullable: false,
            },
            use_slider: true,
            select_options_by_setting: None,
        });
    }
    fields.extend(tycode_profile_session_field());
    SessionSettingsSchema {
        backend_kind: BackendKind::Tycode,
        fields,
    }
}

/// A `profile` Select is published only when named profiles exist; with just
/// the shared settings file there is nothing to choose. Discovery problems
/// hide the field (they are logged) — actual profile resolution still fails
/// visibly at spawn.
fn tycode_profile_session_field() -> Option<SessionSettingField> {
    let home = match tycode_home_dir() {
        Ok(home) => home,
        Err(error) => {
            tracing::warn!("Cannot resolve the Tycode home for profile discovery: {error}");
            return None;
        }
    };
    let profiles = match tycode_config::discover_profiles_in(&home) {
        Ok(profiles) => profiles,
        Err(error) => {
            tracing::warn!("{error}");
            return None;
        }
    };
    if profiles.len() < 2 {
        return None;
    }
    Some(SessionSettingField {
        key: TYCODE_PROFILE_SETTING.to_string(),
        label: "Profile".to_string(),
        description: Some(
            "Tycode settings profile for this session. The default profile is \
             ~/.tycode/settings.toml; named profiles are ~/.tycode/profiles/<name>.toml."
                .to_string(),
        ),
        field_type: SessionSettingFieldType::Select {
            options: profiles
                .iter()
                .map(|profile| select_option(&profile.name, &profile.name))
                .collect(),
            default: Some(tycode_config::TYCODE_DEFAULT_PROFILE.to_string()),
            nullable: false,
        },
        use_slider: false,
        select_options_by_setting: None,
    })
}

pub(crate) fn resolve_session_settings(config: &BackendSpawnConfig) -> SessionSettingsValues {
    let mut resolved = SessionSettingsValues::default();
    if let Some(session_settings) = config.session_settings.as_ref() {
        apply_session_settings_update(&mut resolved, session_settings);
    }
    resolved
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TycodeSettingsOverlayMode {
    SessionRuntime,
    PersistentSettingsPanel,
}

#[cfg(test)]
fn apply_tycode_backend_config_overlay(
    current_settings: &Value,
    config: &BackendConfigValues,
    mode: TycodeSettingsOverlayMode,
) -> Result<TycodeSettingsOverlay, String> {
    apply_tycode_settings_overlay(
        current_settings,
        config,
        &SessionSettingsValues::default(),
        mode,
    )
}

fn apply_tycode_settings_overlay(
    current_settings: &Value,
    config: &BackendConfigValues,
    _session_settings: &SessionSettingsValues,
    mode: TycodeSettingsOverlayMode,
) -> Result<TycodeSettingsOverlay, String> {
    let mut settings = current_settings.clone();
    let object = settings
        .as_object_mut()
        .ok_or_else(|| "Tycode Settings event data must be a JSON object".to_string())?;

    if mode == TycodeSettingsOverlayMode::SessionRuntime
        && object.contains_key("orchestration_progress_messages")
    {
        object.insert(
            "orchestration_progress_messages".to_string(),
            Value::Bool(false),
        );
    }

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
            ("active_provider", SessionSettingValue::Null) => {
                if mode == TycodeSettingsOverlayMode::PersistentSettingsPanel {
                    object.insert("active_provider".to_string(), Value::Null);
                }
            }
            ("model_quality", SessionSettingValue::String(model_quality)) => {
                object.insert(
                    "model_quality".to_string(),
                    Value::String(model_quality.clone()),
                );
            }
            ("model_quality", SessionSettingValue::Null) => {
                if mode == TycodeSettingsOverlayMode::PersistentSettingsPanel {
                    object.insert("model_quality".to_string(), Value::Null);
                }
                continue;
            }
            ("reasoning_effort", SessionSettingValue::String(reasoning_effort)) => {
                object.insert(
                    "reasoning_effort".to_string(),
                    Value::String(reasoning_effort.clone()),
                );
            }
            ("reasoning_effort", SessionSettingValue::Null) => {
                if mode == TycodeSettingsOverlayMode::PersistentSettingsPanel {
                    object.insert("reasoning_effort".to_string(), Value::Null);
                }
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
                if mode == TycodeSettingsOverlayMode::PersistentSettingsPanel {
                    object.insert(key.clone(), tycode_managed_setting_default(key));
                }
                continue;
            }
            ("active_provider", _) => {
                return Err(
                    "Tycode active_provider backend config must be a string or null".to_string(),
                );
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

const TYCODE_MANAGED_SETTINGS: &[&str] = &[
    "active_provider",
    "model_quality",
    "reasoning_effort",
    "autonomy_level",
    "review_level",
    "spawn_context_mode",
];

fn tycode_managed_setting_default(key: &str) -> Value {
    match key {
        "active_provider" | "model_quality" | "reasoning_effort" => Value::Null,
        "autonomy_level" => Value::String("plan_approval_required".to_string()),
        "review_level" => Value::String("None".to_string()),
        "spawn_context_mode" => Value::String("Fork".to_string()),
        _ => unreachable!("unmanaged Tycode setting default requested: {key}"),
    }
}

pub(crate) fn tycode_backend_config_persistence_values(
    incoming: &BackendConfigValues,
    previous: &BackendConfigValues,
) -> BackendConfigValues {
    let mut values = incoming.clone();
    if incoming.0.is_empty() {
        for key in TYCODE_MANAGED_SETTINGS {
            if previous.0.contains_key(*key) {
                values
                    .0
                    .insert((*key).to_string(), SessionSettingValue::Null);
            }
        }
    }
    values
}

pub(crate) fn validate_runtime_session_settings_update(
    update: &SessionSettingsValues,
) -> Result<(), String> {
    if update.0.contains_key("default_agent") {
        return Err(
            "Tycode default_agent cannot be changed on a running session; start a new Tycode \
             session with the desired orchestration setting"
                .to_string(),
        );
    }
    // A running Tycode subprocess is bound to the settings file it was
    // spawned with; the profile cannot change mid-session.
    if update.0.contains_key(TYCODE_PROFILE_SETTING) {
        return Err(
            "Tycode profile cannot be changed on a running session; start a new Tycode session \
             with the desired profile"
                .to_string(),
        );
    }
    Ok(())
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
    AwaitRootAgentChanged {
        agent: String,
    },
    Complete,
}

enum TycodeStartupObservation {
    Allow,
    Suppress,
    Completed,
}

#[derive(Clone, Copy)]
enum TycodeRootAgentOverridePolicy {
    Supported,
    UnsupportedPinnedVersion,
    DisabledForReadOnly,
}

fn tycode_set_root_agent_supported() -> bool {
    #[cfg(test)]
    if let Some(supported) = *TEST_TYCODE_SET_ROOT_AGENT_SUPPORTED
        .lock()
        .expect("test Tycode SetRootAgent support mutex poisoned")
    {
        return supported;
    }

    true
}

fn tycode_root_agent_override_policy(config: &BackendSpawnConfig) -> TycodeRootAgentOverridePolicy {
    if config.resolved_spawn_config.access_mode == BackendAccessMode::ReadOnly {
        return TycodeRootAgentOverridePolicy::DisabledForReadOnly;
    }
    if tycode_set_root_agent_supported() {
        TycodeRootAgentOverridePolicy::Supported
    } else {
        TycodeRootAgentOverridePolicy::UnsupportedPinnedVersion
    }
}

struct TycodeStartupController {
    backend_config: BackendConfigValues,
    session_settings: SessionSettingsValues,
    root_agent_override_policy: TycodeRootAgentOverridePolicy,
    phase: TycodeStartupPhase,
    follow_up: TycodeStartupFollowUp,
    persist_settings: bool,
    runtime_settings: Option<Value>,
}

impl TycodeStartupController {
    fn new(
        backend_config: BackendConfigValues,
        session_settings: SessionSettingsValues,
        root_agent_override_policy: TycodeRootAgentOverridePolicy,
        follow_up: TycodeStartupFollowUp,
        persist_settings: bool,
    ) -> Self {
        Self {
            backend_config,
            session_settings,
            root_agent_override_policy,
            phase: TycodeStartupPhase::AwaitSessionStarted,
            follow_up,
            persist_settings,
            runtime_settings: None,
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
                    let overlay = apply_tycode_settings_overlay(
                        settings,
                        &self.backend_config,
                        &self.session_settings,
                        if self.persist_settings {
                            TycodeSettingsOverlayMode::PersistentSettingsPanel
                        } else {
                            TycodeSettingsOverlayMode::SessionRuntime
                        },
                    )
                    .map_err(|err| format!("Failed to apply Tycode settings overlay: {err}"))?;
                    send_tycode_json(
                        stdin_tx,
                        serde_json::json!({
                            "SaveSettings": {
                                "settings": overlay.settings.clone(),
                                "persist": self.persist_settings,
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
                    self.runtime_settings = Some(settings.clone());
                    if let Some(provider) = active_provider_change.take() {
                        send_tycode_json(
                            stdin_tx,
                            serde_json::json!({ "ChangeProvider": provider }),
                        )?;
                        self.phase = TycodeStartupPhase::AwaitProviderChange { provider };
                    } else {
                        return self.send_root_agent_or_follow_up(stdin_tx);
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
                    return self.send_root_agent_or_follow_up(stdin_tx);
                }
                Ok(tycode_startup_internal_observation(value))
            }
            TycodeStartupPhase::AwaitRootAgentChanged { agent } => {
                if let Some(error) = tycode_error_message(value) {
                    return Err(format!("Tycode SetRootAgent '{agent}' failed: {error}"));
                }
                if let Some(changed_agent) = tycode_root_agent_changed(value) {
                    if changed_agent != agent {
                        return Err(format!(
                            "Tycode SetRootAgent '{agent}' acknowledged unexpected root agent '{changed_agent}'"
                        ));
                    }
                    self.send_follow_up(stdin_tx)?;
                    self.phase = TycodeStartupPhase::Complete;
                    return Ok(TycodeStartupObservation::Completed);
                }
                Ok(tycode_startup_internal_observation(value))
            }
            TycodeStartupPhase::Complete => Ok(TycodeStartupObservation::Allow),
        }
    }

    fn runtime_settings(&self) -> Option<&Value> {
        self.runtime_settings.as_ref()
    }

    fn send_root_agent_or_follow_up(
        &mut self,
        stdin_tx: &mpsc::UnboundedSender<TycodeStdinCommand>,
    ) -> Result<TycodeStartupObservation, String> {
        if let Some(agent) = self.requested_root_agent()? {
            send_tycode_json(
                stdin_tx,
                serde_json::json!({ "SetRootAgent": { "agent": agent } }),
            )?;
            self.phase = TycodeStartupPhase::AwaitRootAgentChanged { agent };
            return Ok(TycodeStartupObservation::Suppress);
        }
        self.send_follow_up(stdin_tx)?;
        self.phase = TycodeStartupPhase::Complete;
        Ok(TycodeStartupObservation::Completed)
    }

    fn requested_root_agent(&self) -> Result<Option<String>, String> {
        if !matches!(self.follow_up, TycodeStartupFollowUp::InitialUserInput(_)) {
            return Ok(None);
        }
        match self.session_settings.0.get("default_agent") {
            Some(SessionSettingValue::String(agent))
                if matches!(agent.as_str(), "one_shot" | "tycode" | "builder" | "swarm") =>
            {
                match self.root_agent_override_policy {
                    TycodeRootAgentOverridePolicy::Supported => Ok(Some(agent.clone())),
                    TycodeRootAgentOverridePolicy::UnsupportedPinnedVersion => Err(format!(
                        "Tycode default_agent session setting requires SetRootAgent support, but \
                         the selected tycode-subprocess does not support that protocol; Tyde \
                         requires Tycode {TYCODE_VERSION}"
                    )),
                    TycodeRootAgentOverridePolicy::DisabledForReadOnly => Err(
                        "Tycode default_agent session setting cannot be used with read-only \
                         Tycode sessions because it would replace Tyde's read-only root agent"
                            .to_string(),
                    ),
                }
            }
            Some(SessionSettingValue::String(agent)) => Err(format!(
                "Tycode default_agent session setting has unsupported value '{agent}'"
            )),
            Some(_) => Err("Tycode default_agent session setting must be a string".to_string()),
            None => Ok(None),
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
            TycodeStartupPhase::AwaitRootAgentChanged { .. } => {
                "waiting for RootAgentChanged acknowledgement"
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

fn unavailable_native_settings_snapshot(message: String) -> BackendNativeSettingsSnapshot {
    BackendNativeSettingsSnapshot {
        backend_kind: BackendKind::Tycode,
        status: BackendConfigSnapshotStatus::Unavailable,
        settings: None,
        groups: Vec::new(),
        message: Some(message),
        advisories: Vec::new(),
    }
}

pub(crate) async fn native_settings_snapshot() -> BackendNativeSettingsSnapshot {
    match probe_native_settings_snapshot().await {
        Ok(snapshot) => snapshot,
        Err(error) => unavailable_native_settings_snapshot(error),
    }
}

/// Probe every discovered profile's settings file through the pinned Tycode
/// subprocess and assemble the per-profile settings document. The form
/// schema (`groups`) is identical for every profile — one pinned binary
/// serves them all — so it rides once in the snapshot's generic field.
async fn probe_native_settings_snapshot() -> Result<BackendNativeSettingsSnapshot, String> {
    let home = tycode_home_dir()?;
    cleanup_retired_projection_artifacts(&home);
    let profiles = tycode_config::discover_profiles_in(&home)?;

    let mut doc = TycodeNativeSettingsDoc {
        version: TYCODE_NATIVE_SETTINGS_VERSION,
        profiles: Vec::new(),
        actions: Vec::new(),
    };
    let mut groups = Vec::new();
    let mut advisories = Vec::new();
    for profile in &profiles {
        let result =
            probe_profile_settings(TycodeCommandPurpose::NativeSettingsProbe, profile).await?;
        let settings = result.snapshot.settings.ok_or_else(|| {
            format!(
                "Tycode settings schema omitted current settings for profile '{}'",
                profile.name
            )
        })?;
        if profile.name == tycode_config::TYCODE_DEFAULT_PROFILE {
            groups = result.snapshot.groups;
        }
        advisories.extend(
            result
                .advisories
                .into_iter()
                .map(|advisory| attribute_advisory_to_profile(advisory, &profile.name)),
        );
        doc.profiles.push(TycodeProfileSettings {
            name: profile.name.clone(),
            settings_path: profile.settings_path.to_string_lossy().to_string(),
            settings,
            base_settings: None,
        });
    }

    Ok(BackendNativeSettingsSnapshot {
        backend_kind: BackendKind::Tycode,
        status: BackendConfigSnapshotStatus::Ready,
        settings: Some(
            serde_json::to_value(&doc)
                .map_err(|err| format!("Failed to encode Tycode profiles document: {err}"))?,
        ),
        groups,
        message: None,
        advisories,
    })
}

/// Advisories from a multi-profile probe merge into one snapshot; name the
/// profile so a diagnostic for a named profile cannot read as one for the
/// shared settings file.
fn attribute_advisory_to_profile(
    advisory: BackendNativeSettingsAdvisory,
    profile_name: &str,
) -> BackendNativeSettingsAdvisory {
    if profile_name == tycode_config::TYCODE_DEFAULT_PROFILE {
        return advisory;
    }
    match advisory {
        BackendNativeSettingsAdvisory::NoProviderConfigured { message } => {
            BackendNativeSettingsAdvisory::NoProviderConfigured {
                message: format!("Profile '{profile_name}': {message}"),
            }
        }
        BackendNativeSettingsAdvisory::BackendReported { message } => {
            BackendNativeSettingsAdvisory::BackendReported {
                message: format!("Profile '{profile_name}': {message}"),
            }
        }
    }
}

async fn probe_profile_settings(
    purpose: TycodeCommandPurpose,
    profile: &tycode_config::TycodeProfileRef,
) -> Result<TycodeSettingsOperationResult, String> {
    let command = tycode_settings_command(purpose, &profile.settings_path).await?;
    run_tycode_settings_operation(command, purpose, TycodeSettingsOperation::Probe).await
}

/// Persist the edited profiles document: profile file operations first, then
/// each changed profile's settings saved by the Tycode subprocess against
/// that profile's real settings file. A save based on a stale snapshot is
/// refused, never merged or last-writer-wins.
pub(crate) async fn persist_native_settings(settings: Value) -> Result<(), String> {
    let doc: TycodeNativeSettingsDoc = serde_json::from_value(settings)
        .map_err(|err| format!("invalid Tycode settings document: {err}"))?;
    if doc.version != TYCODE_NATIVE_SETTINGS_VERSION {
        return Err(format!(
            "unsupported Tycode settings document version {} (expected {})",
            doc.version, TYCODE_NATIVE_SETTINGS_VERSION
        ));
    }
    let home = tycode_home_dir()?;

    // Saves are serialized so two clients cannot interleave their
    // check-then-write sequences and silently overwrite each other.
    static TYCODE_PERSIST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _persist_guard = TYCODE_PERSIST_LOCK.lock().await;

    // Profile file operations first, so a profile created here can be edited
    // and saved by the same document.
    for action in &doc.actions {
        match action {
            TycodeProfileAction::CreateProfile { name, copy_from } => {
                tycode_config::create_profile_in(&home, name, copy_from.as_deref())?;
            }
            TycodeProfileAction::DeleteProfile { name } => {
                tycode_config::delete_profile_in(&home, name)?;
            }
        }
    }

    for profile_settings in &doc.profiles {
        let profile = tycode_config::resolve_profile_ref_in(&home, Some(&profile_settings.name))?;
        let current = probe_profile_settings(TycodeCommandPurpose::NativeSettingsPersist, &profile)
            .await?
            .snapshot
            .settings
            .ok_or_else(|| {
                format!(
                    "Tycode settings schema omitted current settings for profile '{}'",
                    profile.name
                )
            })?;
        if current == profile_settings.settings {
            continue;
        }
        // A changed profile without its base is an unverifiable save — refuse
        // it rather than fall back to last-writer-wins.
        let Some(base) = &profile_settings.base_settings else {
            return Err(format!(
                "Tycode profile '{}' settings update is missing its base settings; \
                 reload the settings and try again",
                profile_settings.name
            ));
        };
        if current != *base {
            return Err(format!(
                "Tycode profile '{}' settings changed since they were loaded; \
                 reload the settings and re-apply your edits",
                profile_settings.name
            ));
        }
        save_profile_settings(&profile, profile_settings.settings.clone()).await?;
    }
    Ok(())
}

/// Run one `SaveSettings { persist: true }` conversation against the
/// profile's real settings file; the Tycode subprocess validates the payload
/// and owns the write.
async fn save_profile_settings(
    profile: &tycode_config::TycodeProfileRef,
    settings: Value,
) -> Result<(), String> {
    if !settings.is_object() {
        return Err("Tycode native settings must be a JSON object".to_string());
    }
    let purpose = TycodeCommandPurpose::NativeSettingsPersist;
    let command = tycode_settings_command(purpose, &profile.settings_path).await?;
    run_tycode_settings_operation(command, purpose, TycodeSettingsOperation::Save(settings))
        .await
        .map(|_| ())
}

pub(crate) async fn persist_backend_config(values: BackendConfigValues) -> Result<(), String> {
    if values.0.is_empty() {
        return Ok(());
    }
    let home = tycode_home_dir()?;
    let profile = tycode_config::resolve_profile_ref_in(&home, None)?;
    let probed = probe_profile_settings(TycodeCommandPurpose::LegacyConfigProbe, &profile).await?;
    let settings = probed
        .snapshot
        .settings
        .ok_or_else(|| "Tycode settings schema omitted current settings".to_string())?;
    let overlay = apply_tycode_settings_overlay(
        &settings,
        &values,
        &SessionSettingsValues::default(),
        TycodeSettingsOverlayMode::PersistentSettingsPanel,
    )
    .map_err(|err| format!("Failed to apply Tycode settings overlay: {err}"))?;
    let purpose = TycodeCommandPurpose::LegacyConfigPersist;
    let command = tycode_settings_command(purpose, &profile.settings_path).await?;
    run_tycode_settings_operation(
        command,
        purpose,
        TycodeSettingsOperation::Save(overlay.settings),
    )
    .await
    .map(|_| ())
}

#[cfg(test)]
pub(crate) async fn backend_config_snapshot() -> Result<BackendConfigValues, String> {
    let home = tycode_home_dir()?;
    let profile = tycode_config::resolve_profile_ref_in(&home, None)?;
    let probed = probe_profile_settings(TycodeCommandPurpose::LegacyConfigProbe, &profile).await?;
    let settings = probed
        .snapshot
        .settings
        .ok_or_else(|| "Tycode settings schema omitted current settings".to_string())?;
    Ok(tycode_backend_config_snapshot_values(&settings))
}

#[cfg(test)]
fn tycode_backend_config_snapshot_values(settings: &Value) -> BackendConfigValues {
    let mut values = BackendConfigValues::default();
    for key in TYCODE_MANAGED_SETTINGS {
        let Some(value) = settings.get(*key) else {
            continue;
        };
        let setting = match value {
            Value::String(value) if !value.trim().is_empty() => {
                SessionSettingValue::String(value.clone())
            }
            Value::Null => SessionSettingValue::Null,
            _ => continue,
        };
        values.0.insert((*key).to_string(), setting);
    }
    values
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

fn send_tycode_runtime_session_settings_update(
    runtime_settings: &mut Option<Value>,
    update: &SessionSettingsValues,
    stdin_tx: &mpsc::UnboundedSender<TycodeStdinCommand>,
) -> Result<(), String> {
    validate_runtime_session_settings_update(update)?;
    let current_settings = runtime_settings.as_ref().ok_or_else(|| {
        "Tycode runtime settings unavailable while applying session settings update".to_string()
    })?;
    let overlay = apply_tycode_settings_overlay(
        current_settings,
        &BackendConfigValues::default(),
        update,
        TycodeSettingsOverlayMode::SessionRuntime,
    )
    .map_err(|err| format!("Failed to apply Tycode session settings update: {err}"))?;
    send_tycode_json(
        stdin_tx,
        serde_json::json!({
            "SaveSettings": {
                "settings": overlay.settings.clone(),
                "persist": false,
            }
        }),
    )?;
    *runtime_settings = Some(overlay.settings);
    Ok(())
}

fn tycode_settings_data(value: &Value) -> Option<&Value> {
    (value.get("kind").and_then(Value::as_str) == Some("Settings"))
        .then(|| value.get("data"))
        .flatten()
}

fn tycode_settings_schema_data(value: &Value) -> Option<&Value> {
    (value.get("kind").and_then(Value::as_str) == Some("SettingsSchema"))
        .then(|| value.get("data"))
        .flatten()
        .and_then(|data| data.get("schema"))
}

fn tycode_native_settings_snapshot_from_schema(
    schema: &Value,
) -> Result<BackendNativeSettingsSnapshot, String> {
    let settings = schema
        .get("settings")
        .cloned()
        .ok_or_else(|| "Tycode SettingsSchema event missing current settings".to_string())?;
    if !settings.is_object() {
        return Err("Tycode SettingsSchema current settings must be an object".to_string());
    }
    let groups_value = schema
        .get("groups")
        .cloned()
        .ok_or_else(|| "Tycode SettingsSchema event missing groups".to_string())?;
    let groups = serde_json::from_value::<Vec<BackendNativeSettingsGroup>>(groups_value)
        .map_err(|err| format!("Failed to parse Tycode SettingsSchema groups: {err}"))?;

    Ok(BackendNativeSettingsSnapshot {
        backend_kind: BackendKind::Tycode,
        status: BackendConfigSnapshotStatus::Ready,
        settings: Some(settings),
        groups,
        message: None,
        advisories: Vec::new(),
    })
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

fn tycode_root_agent_changed(value: &Value) -> Option<&str> {
    (value.get("kind").and_then(Value::as_str) == Some("RootAgentChanged"))
        .then(|| {
            value
                .get("data")
                .and_then(|data| data.get("agent"))
                .and_then(Value::as_str)
        })
        .flatten()
}

fn tycode_startup_internal_observation(value: &Value) -> TycodeStartupObservation {
    match value.get("kind").and_then(Value::as_str) {
        Some("Settings" | "TimingUpdate" | "TypingStatusChanged" | "RootAgentChanged") => {
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
        "orchestration_progress_messages",
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
        "orchestration_progress_messages",
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
        tycode_session_settings_schema()
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
            let mut command = match tycode_session_command(
                TycodeCommandPurpose::NewSession,
                &config,
                &roots_json,
            )
            .await
            {
                Ok(command) => command,
                Err(err) => {
                    tracing::error!("{err}");
                    let _ = ready_tx.send(Err(err));
                    return;
                }
            };
            if let Some(agent_json) = tycode_read_only_agent_json(&config) {
                command.arg("--agent").arg(agent_json);
            }
            if let Some(mcp_servers_json) = mcp_servers_json.as_deref() {
                command.arg("--mcp-servers").arg(mcp_servers_json);
            }

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

            let (settings_update_tx, mut settings_update_rx) =
                mpsc::unbounded_channel::<SessionSettingsValues>();

            let mut startup = TycodeStartupController::new(
                config.backend_config.clone(),
                resolve_session_settings(&config),
                tycode_root_agent_override_policy(&config),
                TycodeStartupFollowUp::InitialUserInput(initial_message),
                false,
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
                        AgentInput::UpdateSessionSettings(payload) => {
                            if settings_update_tx.send(payload.values).is_err() {
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
            let mut stream_state = TycodeStreamState::default();
            let mut runtime_settings = None;
            let mut settings_updates_open = true;
            let mut ready_tx = Some(ready_tx);
            #[cfg(test)]
            observe_tycode_startup_process_spawned(child.inner().id());
            loop {
                let line = tokio::select! {
                    line = lines.next_line() => line,
                    settings_update = settings_update_rx.recv(), if settings_updates_open => {
                        let Some(settings_update) = settings_update else {
                            settings_updates_open = false;
                            continue;
                        };
                        if let Err(err) = send_tycode_runtime_session_settings_update(
                            &mut runtime_settings,
                            &settings_update,
                            &stdin_tx,
                        ) {
                            tracing::error!("{err}");
                            let _ = events_tx.send(tycode_error_chat_event(err));
                        }
                        continue;
                    }
                    _ = shutdown_rx.recv() => {
                        let _ = child.start_kill();
                        let _ = child.wait().await;
                        #[cfg(test)]
                        observe_tycode_startup_process_reaped();
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
                            event = %tycode_line_diagnostic(trimmed),
                            "Failed to parse tycode-subprocess event: {err}"
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
                        runtime_settings = startup.runtime_settings().cloned();
                        if let Some(ready_tx) = ready_tx.take() {
                            let _ = ready_tx.send(Ok(()));
                        }
                        continue;
                    }
                }

                if let Some(settings) = tycode_settings_data(&value) {
                    runtime_settings = Some(settings.clone());
                }

                let events = map_tycode_value_to_chat_events(&value);
                if events.is_empty() {
                    continue;
                }

                for event in tycode_events_with_synthesized_completion(events, &mut stream_state) {
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
            if stream_state.open {
                let _ = events_tx.send(stream_state.synthetic_stream_end());
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
            let mut command = match tycode_session_command(
                TycodeCommandPurpose::ResumeSession,
                &config,
                &roots_json,
            )
            .await
            {
                Ok(command) => command,
                Err(err) => {
                    tracing::error!("{err}");
                    let _ = ready_tx.send(Err(err));
                    return;
                }
            };
            if let Some(agent_json) = tycode_read_only_agent_json(&config) {
                command.arg("--agent").arg(agent_json);
            }
            if let Some(mcp_servers_json) = mcp_servers_json.as_deref() {
                command.arg("--mcp-servers").arg(mcp_servers_json);
            }

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

            let (settings_update_tx, mut settings_update_rx) =
                mpsc::unbounded_channel::<SessionSettingsValues>();

            let mut startup = TycodeStartupController::new(
                config.backend_config.clone(),
                resolve_session_settings(&config),
                tycode_root_agent_override_policy(&config),
                TycodeStartupFollowUp::ResumeSession {
                    session_id: session_id.0.clone(),
                },
                false,
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
                        AgentInput::UpdateSessionSettings(payload) => {
                            if settings_update_tx.send(payload.values).is_err() {
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
            let mut stream_state = TycodeStreamState::default();
            let mut runtime_settings = None;
            let mut settings_updates_open = true;
            let mut replay_barrier =
                TycodeResumeReplayBarrier::new(session_id.0.clone(), replay_event_count);
            let mut resume_replay_complete_tx = Some(resume_replay_complete_tx);
            let mut ready_tx = Some(ready_tx);
            #[cfg(test)]
            observe_tycode_startup_process_spawned(child.inner().id());
            loop {
                let line = tokio::select! {
                    line = lines.next_line() => line,
                    settings_update = settings_update_rx.recv(), if settings_updates_open => {
                        let Some(settings_update) = settings_update else {
                            settings_updates_open = false;
                            continue;
                        };
                        if let Err(err) = send_tycode_runtime_session_settings_update(
                            &mut runtime_settings,
                            &settings_update,
                            &stdin_tx,
                        ) {
                            tracing::error!("{err}");
                            let _ = events_tx.send(tycode_error_chat_event(err));
                        }
                        continue;
                    }
                    _ = shutdown_rx.recv() => {
                        let _ = child.start_kill();
                        let _ = child.wait().await;
                        #[cfg(test)]
                        observe_tycode_startup_process_reaped();
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
                            event = %tycode_line_diagnostic(trimmed),
                            "Failed to parse tycode-subprocess resume event: {err}"
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
                        runtime_settings = startup.runtime_settings().cloned();
                        if let Some(ready_tx) = ready_tx.take() {
                            let _ = ready_tx.send(Ok(()));
                        }
                        continue;
                    }
                }

                if let Some(settings) = tycode_settings_data(&value) {
                    runtime_settings = Some(settings.clone());
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

                for event in tycode_events_with_synthesized_completion(events, &mut stream_state) {
                    if events_tx.send(event).is_err() {
                        break;
                    }
                }
            }

            if stream_state.open {
                let _ = events_tx.send(stream_state.synthetic_stream_end());
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
            let diagnostic = tycode_text_diagnostic(trimmed);
            tracing::warn!(stderr = %diagnostic, "tycode-subprocess stderr");
            *sink.lock().expect("tycode stderr mutex poisoned") = Some(diagnostic);
        }
    });
    last_stderr_line
}

const TYCODE_DIAGNOSTIC_PREVIEW_CHARS: usize = 240;

fn tycode_line_diagnostic(line: &str) -> String {
    if let Some(kind) = extract_json_string_field(line, "kind")
        && matches!(
            kind.as_str(),
            "Settings"
                | "SettingsSchema"
                | "MessageAdded"
                | "StreamDelta"
                | "StreamReasoningDelta"
                | "StreamEnd"
        )
    {
        return format!("{{\"kind\":\"{kind}\",\"data\":\"<redacted>\"}}");
    }

    tycode_text_diagnostic(line)
}

fn tycode_event_diagnostic(value: &Value) -> String {
    tycode_diagnostic_preview(
        &serde_json::to_string(&sanitize_tycode_value_for_diagnostics(value))
            .unwrap_or_else(|_| "<unserializable Tycode event>".to_string()),
    )
}

fn sanitize_tycode_value_for_diagnostics(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut sanitized = serde_json::Map::new();
            for (key, value) in map {
                if tycode_diagnostic_key_is_sensitive(key) {
                    sanitized.insert(key.clone(), Value::String("<redacted>".to_string()));
                } else {
                    sanitized.insert(key.clone(), sanitize_tycode_value_for_diagnostics(value));
                }
            }
            Value::Object(sanitized)
        }
        Value::Array(values) => Value::Array(
            values
                .iter()
                .map(sanitize_tycode_value_for_diagnostics)
                .collect(),
        ),
        Value::String(value) => Value::String(tycode_text_diagnostic(value)),
        _ => value.clone(),
    }
}

fn tycode_diagnostic_key_is_sensitive(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    matches!(
        key.as_str(),
        "api_key"
            | "apikey"
            | "authorization"
            | "bearer"
            | "content"
            | "credential"
            | "credentials"
            | "images"
            | "input"
            | "arguments"
            | "message"
            | "password"
            | "prompt"
            | "providers"
            | "reasoning"
            | "secret"
            | "settings"
            | "text"
            | "token"
            | "tool_calls"
            | "userinput"
            | "savesettings"
    ) || key.ends_with("_key")
        || key.ends_with("_token")
}

fn tycode_text_diagnostic(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let lower = trimmed.to_ascii_lowercase();
    for marker in [
        "api_key",
        "apikey",
        "authorization",
        "bearer",
        "password",
        "secret",
        "token",
        "credential",
        "userinput",
        "save_settings",
        "savesettings",
    ] {
        if let Some(index) = lower.find(marker) {
            return tycode_diagnostic_preview(&format!(
                "{} <redacted>",
                trimmed[..index + marker.len()].trim_end()
            ));
        }
    }

    tycode_diagnostic_preview(trimmed)
}

fn tycode_diagnostic_preview(text: &str) -> String {
    let mut preview = String::new();
    let mut chars = text.chars();
    for _ in 0..TYCODE_DIAGNOSTIC_PREVIEW_CHARS {
        let Some(ch) = chars.next() else {
            return preview;
        };
        preview.push(ch);
    }
    if chars.next().is_some() {
        preview.push('…');
    }
    preview
}

fn extract_json_string_field(line: &str, field: &str) -> Option<String> {
    let needle = format!("\"{field}\"");
    let field_start = line.find(&needle)?;
    let after_field = &line[field_start + needle.len()..];
    let colon_index = after_field.find(':')?;
    let after_colon = after_field[colon_index + 1..].trim_start();
    let mut chars = after_colon.chars();
    if chars.next()? != '"' {
        return None;
    }
    let mut value = String::new();
    let mut escaped = false;
    for ch in chars {
        if escaped {
            value.push(ch);
            escaped = false;
            continue;
        }
        match ch {
            '\\' => escaped = true,
            '"' => return Some(value),
            _ => value.push(ch),
        }
    }
    None
}

fn tycode_startup_exit_error(last_stderr_line: &Arc<std::sync::Mutex<Option<String>>>) -> String {
    tycode_process_exit_error(
        last_stderr_line,
        "Tycode process exited before reporting a session_id",
    )
}

fn tycode_process_exit_error(
    last_stderr_line: &Arc<std::sync::Mutex<Option<String>>>,
    message: &str,
) -> String {
    match last_stderr_line
        .lock()
        .expect("tycode stderr mutex poisoned")
        .clone()
    {
        Some(stderr) => format!("{message}: {stderr}"),
        None => message.to_string(),
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
    if value.get("kind").and_then(Value::as_str) == Some("Orchestration") {
        return map_tycode_orchestration_event(value);
    }

    let normalized = normalize_tycode_event_value(value);
    if let Ok(event) = serde_json::from_value::<ChatEvent>(normalized) {
        return vec![event];
    }

    let Some(kind) = value.get("kind").and_then(Value::as_str) else {
        tracing::warn!(
            event = %tycode_event_diagnostic(value),
            "Ignoring Tycode event without kind"
        );
        return Vec::new();
    };

    if is_known_tycode_typed_chat_event_kind(kind) {
        let err = serde_json::from_value::<ChatEvent>(normalize_tycode_event_value(value))
            .expect_err("known Tycode event failed to deserialize above");
        tracing::error!(
            kind,
            error = %err,
            event = %tycode_event_diagnostic(value),
            "Malformed Tycode chat event"
        );
        let error_event = tycode_error_chat_event(format!("Malformed Tycode {kind} event: {err}"));
        if kind == "StreamEnd" {
            return vec![error_event, tycode_malformed_stream_end_event()];
        }
        return vec![error_event];
    }

    match kind {
        "Settings"
        | "SettingsSchema"
        | "ConversationCleared"
        | "SessionsList"
        | "ProfilesList"
        | "TimingUpdate"
        | "ModuleSchemas"
        | "SessionStarted"
        | "RootAgentChanged" => Vec::new(),
        "Error" => {
            let Some(message) = value.get("data").and_then(Value::as_str) else {
                tracing::error!(
                    event = %tycode_event_diagnostic(value),
                    "Malformed Tycode Error event"
                );
                return vec![tycode_error_chat_event(
                    "Malformed Tycode Error event: data must be a string",
                )];
            };
            vec![tycode_error_chat_event(message)]
        }
        other => {
            tracing::warn!(
                kind = %other,
                event = %tycode_event_diagnostic(value),
                "Ignoring unsupported Tycode event"
            );
            Vec::new()
        }
    }
}

fn normalize_tycode_event_value(value: &Value) -> Value {
    let mut normalized = value.clone();
    match normalized.get("kind").and_then(Value::as_str) {
        Some("MessageAdded") => {
            if let Some(message) = normalized.get_mut("data") {
                normalize_tycode_chat_message(message);
            }
        }
        Some("StreamEnd") => {
            if let Some(message) = normalized
                .get_mut("data")
                .and_then(|data| data.get_mut("message"))
            {
                normalize_tycode_chat_message(message);
            }
        }
        _ => {}
    }
    normalized
}

fn normalize_tycode_chat_message(message: &mut Value) {
    let Some(token_usage) = message.get_mut("token_usage") else {
        return;
    };
    let Value::Object(usage) = token_usage else {
        return;
    };
    if usage.contains_key("request")
        || usage.contains_key("turn")
        || usage.contains_key("cumulative")
        || !(usage.contains_key("input_tokens")
            && usage.contains_key("output_tokens")
            && usage.contains_key("total_tokens"))
    {
        return;
    }

    let flat_usage = Value::Object(usage.clone());
    *token_usage = serde_json::json!({
        "request": {
            "kind": "known",
            "usage": flat_usage.clone(),
        },
        "turn": {
            "kind": "known",
            "usage": flat_usage,
        },
        "cumulative": {
            "kind": "unavailable",
            "reason": "backend_did_not_report",
        },
    });
}

fn is_known_tycode_typed_chat_event_kind(kind: &str) -> bool {
    matches!(
        kind,
        "MessageAdded"
            | "TypingStatusChanged"
            | "StreamStart"
            | "StreamDelta"
            | "StreamReasoningDelta"
            | "StreamEnd"
            | "ToolRequest"
            | "ToolExecutionCompleted"
            | "OperationCancelled"
            | "RetryAttempt"
            | "TaskUpdate"
    )
}

fn map_tycode_orchestration_event(value: &Value) -> Vec<ChatEvent> {
    let Some(payload_kind) = value
        .get("data")
        .and_then(|data| data.get("payload"))
        .and_then(|payload| payload.get("kind"))
        .and_then(Value::as_str)
    else {
        tracing::error!(
            event = %tycode_event_diagnostic(value),
            "Malformed Tycode Orchestration event missing payload kind"
        );
        return vec![tycode_error_chat_event(
            "Malformed Tycode Orchestration event: missing data.payload.kind",
        )];
    };

    if !is_known_tycode_orchestration_payload_kind(payload_kind) {
        tracing::warn!(
            payload_kind,
            event = %tycode_event_diagnostic(value),
            "Ignoring unknown Tycode Orchestration payload kind"
        );
        return Vec::new();
    }

    match value
        .get("data")
        .cloned()
        .ok_or_else(|| "missing data".to_string())
        .and_then(|data| {
            serde_json::from_value::<OrchestrationEvent>(data)
                .map_err(|err| format!("failed to parse {payload_kind}: {err}"))
        }) {
        Ok(event) => vec![ChatEvent::Orchestration(event)],
        Err(err) => {
            tracing::error!(
                payload_kind,
                error = %err,
                event = %tycode_event_diagnostic(value),
                "Malformed Tycode Orchestration event"
            );
            vec![tycode_error_chat_event(format!(
                "Malformed Tycode Orchestration event ({payload_kind}): {err}"
            ))]
        }
    }
}

fn is_known_tycode_orchestration_payload_kind(kind: &str) -> bool {
    matches!(
        kind,
        "AgentStarted"
            | "AgentCompleted"
            | "PhaseChanged"
            | "FanOutStarted"
            | "WorkerStarted"
            | "WorkerCompleted"
            | "FanOutCompleted"
            | "ConsensusRoundResolved"
            | "PlanSelected"
            | "ReviewRoundResolved"
    )
}

fn tycode_error_chat_event(message: impl Into<String>) -> ChatEvent {
    ChatEvent::MessageAdded(ChatMessage {
        message_id: None,
        timestamp: unix_now_ms(),
        sender: MessageSender::Error,
        content: message.into(),
        reasoning: None,
        tool_calls: Vec::new(),
        model_info: None,
        token_usage: None,
        context_breakdown: None,
        images: None,
    })
}

fn tycode_malformed_stream_end_event() -> ChatEvent {
    tycode_stream_end_event(String::new())
}

fn tycode_stream_end_event(content: String) -> ChatEvent {
    ChatEvent::StreamEnd(StreamEndData {
        message: ChatMessage {
            message_id: None,
            timestamp: unix_now_ms(),
            sender: MessageSender::Assistant {
                agent: "tycode".to_string(),
            },
            content,
            reasoning: None,
            tool_calls: Vec::new(),
            model_info: None,
            token_usage: None,
            context_breakdown: None,
            images: None,
        },
    })
}

#[derive(Debug, Default)]
struct TycodeStreamState {
    open: bool,
    message_id: Option<String>,
    agent: Option<String>,
    model: Option<String>,
    accumulated_text: String,
    accumulated_reasoning: String,
    synthetic_completion: Option<SyntheticTycodeCompletion>,
}

#[derive(Debug)]
struct SyntheticTycodeCompletion {
    message_id: Option<ChatMessageId>,
    content: String,
    reasoning_text: Option<String>,
}

impl TycodeStreamState {
    fn events_with_synthesized_completion(&mut self, events: Vec<ChatEvent>) -> Vec<ChatEvent> {
        let mut output = Vec::new();
        for mut event in events {
            if let Some(events) = self.late_authoritative_stream_end_events(&event) {
                output.extend(events);
                continue;
            }
            if let Some(stream_end) = self.synthesize_stream_end_before(&event) {
                output.push(stream_end);
            }
            self.inject_stream_identity(&mut event);
            self.update(&event);
            output.push(event);
        }

        output
    }

    /// The Tycode wire predates Tyde's stream identity contract: `StreamEnd`
    /// carries no message id at all, and start/delta ids are advisory. Tyde
    /// validators reject id-less stream frames, so the adapter must own the
    /// translation: adopt the open stream's id for id-less frames and mint one
    /// when Tycode never provided an id for the stream.
    fn inject_stream_identity(&mut self, event: &mut ChatEvent) {
        match event {
            ChatEvent::StreamStart(start) => {
                if start
                    .message_id
                    .as_ref()
                    .is_none_or(|message_id| message_id.trim().is_empty())
                {
                    start.message_id = Some(minted_tycode_message_id());
                }
            }
            ChatEvent::StreamDelta(delta) | ChatEvent::StreamReasoningDelta(delta) => {
                if self.open
                    && delta
                        .message_id
                        .as_ref()
                        .is_none_or(|message_id| message_id.trim().is_empty())
                {
                    delta.message_id.clone_from(&self.message_id);
                }
            }
            ChatEvent::StreamEnd(end) if self.open && end.message.message_id.is_none() => {
                end.message.message_id = self.message_id.clone().map(ChatMessageId);
            }
            _ => {}
        }
    }

    fn late_authoritative_stream_end_events(
        &mut self,
        event: &ChatEvent,
    ) -> Option<Vec<ChatEvent>> {
        let ChatEvent::StreamEnd(end) = event else {
            return None;
        };
        if self.open {
            return None;
        }
        let synthetic = self.synthetic_completion.take()?;
        self.warn_if_late_stream_end_has_unmerged_fields(&synthetic, &end.message);

        let message_id = synthetic
            .message_id
            .clone()
            .or_else(|| end.message.message_id.clone());
        let Some(message_id) = message_id else {
            tracing::warn!(
                "Forwarding delayed Tycode StreamEnd after synthesized completion because no \
                 message_id is available for metadata merge"
            );
            return Some(vec![event.clone()]);
        };

        if end.message.model_info.is_none()
            && end.message.token_usage.is_none()
            && end.message.context_breakdown.is_none()
        {
            return Some(Vec::new());
        }

        Some(vec![ChatEvent::MessageMetadataUpdated(
            MessageMetadataUpdateData {
                message_id,
                model_info: end.message.model_info.clone(),
                token_usage: end.message.token_usage.clone(),
                context_breakdown: end.message.context_breakdown.clone(),
            },
        )])
    }

    fn synthesize_stream_end_before(&mut self, event: &ChatEvent) -> Option<ChatEvent> {
        if matches!(event, ChatEvent::TypingStatusChanged(false)) && self.open {
            let stream_end = self.synthetic_stream_end();
            if let ChatEvent::StreamEnd(end) = &stream_end {
                self.synthetic_completion = Some(SyntheticTycodeCompletion {
                    message_id: end.message.message_id.clone(),
                    content: end.message.content.clone(),
                    reasoning_text: end
                        .message
                        .reasoning
                        .as_ref()
                        .map(|reasoning| reasoning.text.clone()),
                });
            }
            self.open = false;
            return Some(stream_end);
        }

        None
    }

    fn synthetic_stream_end(&self) -> ChatEvent {
        ChatEvent::StreamEnd(StreamEndData {
            message: ChatMessage {
                message_id: self.message_id.clone().map(ChatMessageId),
                timestamp: unix_now_ms(),
                sender: MessageSender::Assistant {
                    agent: self.agent.clone().unwrap_or_else(|| "tycode".to_string()),
                },
                content: self.accumulated_text.clone(),
                reasoning: (!self.accumulated_reasoning.is_empty()).then(|| ReasoningData {
                    text: self.accumulated_reasoning.clone(),
                    tokens: None,
                    signature: None,
                    blob: None,
                }),
                tool_calls: Vec::new(),
                model_info: self.model.clone().map(|model| ModelInfo { model }),
                token_usage: None,
                context_breakdown: None,
                images: None,
            },
        })
    }

    fn update(&mut self, event: &ChatEvent) {
        match event {
            ChatEvent::TypingStatusChanged(true) | ChatEvent::StreamStart(_) => {
                if let ChatEvent::StreamStart(start) = event {
                    self.open = true;
                    self.message_id.clone_from(&start.message_id);
                    self.agent = Some(start.agent.clone());
                    self.model.clone_from(&start.model);
                    self.accumulated_text.clear();
                    self.accumulated_reasoning.clear();
                }
                self.synthetic_completion = None;
            }
            ChatEvent::StreamDelta(StreamTextDeltaData { message_id, text }) if self.open => {
                if let Some(message_id) = message_id {
                    self.message_id = Some(message_id.clone());
                }
                self.accumulated_text.push_str(text);
            }
            ChatEvent::StreamReasoningDelta(StreamTextDeltaData {
                message_id, text, ..
            }) if self.open => {
                if let Some(message_id) = message_id {
                    self.message_id = Some(message_id.clone());
                }
                self.accumulated_reasoning.push_str(text);
            }
            ChatEvent::StreamEnd(_) => {
                self.open = false;
                self.synthetic_completion = None;
            }
            _ => {}
        }
    }

    fn warn_if_late_stream_end_has_unmerged_fields(
        &self,
        synthetic: &SyntheticTycodeCompletion,
        message: &ChatMessage,
    ) {
        let authoritative_reasoning = message
            .reasoning
            .as_ref()
            .map(|reasoning| reasoning.text.as_str());
        if message.content != synthetic.content
            || authoritative_reasoning != synthetic.reasoning_text.as_deref()
            || !message.tool_calls.is_empty()
            || message
                .images
                .as_ref()
                .is_some_and(|images| !images.is_empty())
        {
            tracing::warn!(
                message_id = ?message.message_id,
                "Delayed Tycode StreamEnd after synthesized completion contains content fields \
                 that cannot be merged into the already visible assistant message without a \
                 duplicate StreamEnd"
            );
        }
    }
}

fn tycode_events_with_synthesized_completion(
    events: Vec<ChatEvent>,
    stream_state: &mut TycodeStreamState,
) -> Vec<ChatEvent> {
    stream_state.events_with_synthesized_completion(events)
}

fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn minted_tycode_message_id() -> String {
    format!("tycode-unidentified-{}", uuid::Uuid::new_v4())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};

    use super::*;
    use protocol::{
        OrchestrationAgentOrigin, OrchestrationPayload, SendMessagePayload, TokenUsageScope,
        TokenUsageUnavailableReason,
    };
    use tempfile::TempDir;

    const TEST_TYCODE_STARTUP_TIMEOUT_DURATION: Duration = Duration::from_secs(2);

    #[test]
    fn tycode_session_settings_schema_omits_orchestration_until_supported() {
        let home = TempDir::new().expect("tempdir");
        let _guard = TestTycodeSchemaGuard::set(home.path(), false);

        let schema = tycode_session_settings_schema();
        assert_eq!(schema.backend_kind, BackendKind::Tycode);
        assert!(schema.fields.is_empty());
    }

    #[test]
    fn tycode_session_settings_schema_exposes_orchestration_slider_when_supported() {
        let home = TempDir::new().expect("tempdir");
        let _guard = TestTycodeSchemaGuard::set(home.path(), true);

        let schema = tycode_session_settings_schema();
        assert_eq!(schema.backend_kind, BackendKind::Tycode);
        assert_eq!(schema.fields.len(), 1);
        let field = &schema.fields[0];
        assert_eq!(field.key, "default_agent");
        assert_eq!(field.label, "Orchestration");
        assert!(field.use_slider);
        match &field.field_type {
            SessionSettingFieldType::Select {
                options,
                default,
                nullable,
            } => {
                assert_eq!(default.as_deref(), Some("tycode"));
                assert!(!nullable);
                assert_eq!(
                    options
                        .iter()
                        .map(|option| (option.value.as_str(), option.label.as_str()))
                        .collect::<Vec<_>>(),
                    vec![
                        ("one_shot", "None"),
                        ("tycode", "Auto"),
                        ("builder", "Pipeline"),
                        ("swarm", "Swarm"),
                    ]
                );
            }
            other => panic!("default_agent should be Select, got {other:?}"),
        }
    }

    #[test]
    fn tycode_resolve_session_settings_keeps_only_explicit_root_agent() {
        let default_config = BackendSpawnConfig::default();
        assert!(resolve_session_settings(&default_config).0.is_empty());

        let mut config = BackendSpawnConfig {
            session_settings: Some(SessionSettingsValues::default()),
            ..Default::default()
        };
        config
            .session_settings
            .as_mut()
            .expect("session settings")
            .0
            .insert(
                "default_agent".to_string(),
                SessionSettingValue::String("tycode".to_string()),
            );
        assert_eq!(
            resolve_session_settings(&config).0.get("default_agent"),
            Some(&SessionSettingValue::String("tycode".to_string()))
        );
    }

    #[test]
    fn tycode_omits_legacy_backend_config_schema() {
        assert!(TycodeBackend::backend_config_schema().is_none());
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
            "orchestration_progress_messages": true,
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

        let overlay = apply_tycode_backend_config_overlay(
            &settings,
            &config,
            TycodeSettingsOverlayMode::SessionRuntime,
        )
        .expect("overlay settings");
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
        assert_eq!(overlay.settings["orchestration_progress_messages"], false);
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

        let overlay = apply_tycode_backend_config_overlay(
            &settings,
            &config,
            TycodeSettingsOverlayMode::SessionRuntime,
        )
        .expect("overlay settings");
        assert_eq!(overlay.settings, settings);
        assert_eq!(overlay.active_provider_change, None);
    }

    #[test]
    fn tycode_settings_overlay_keeps_default_agent_out_of_save_settings() {
        let settings = serde_json::json!({
            "active_provider": "default",
            "providers": {
                "default": { "type": "mock" }
            },
            "default_agent": "tycode",
            "orchestration_progress_messages": true
        });
        let mut session_settings = SessionSettingsValues::default();
        session_settings.0.insert(
            "default_agent".to_string(),
            SessionSettingValue::String("swarm".to_string()),
        );

        let overlay = apply_tycode_settings_overlay(
            &settings,
            &BackendConfigValues::default(),
            &session_settings,
            TycodeSettingsOverlayMode::SessionRuntime,
        )
        .expect("overlay settings");

        assert_eq!(overlay.settings["default_agent"], "tycode");
        assert_eq!(overlay.settings["orchestration_progress_messages"], false);
    }

    #[test]
    fn tycode_runtime_session_settings_reject_default_agent_update() {
        let (stdin_tx, mut stdin_rx) = mpsc::unbounded_channel::<TycodeStdinCommand>();
        let mut runtime_settings = Some(serde_json::json!({
            "active_provider": "default",
            "providers": {
                "default": { "type": "mock" }
            },
            "default_agent": "tycode",
            "orchestration_progress_messages": true
        }));
        let mut update = SessionSettingsValues::default();
        update.0.insert(
            "default_agent".to_string(),
            SessionSettingValue::String("swarm".to_string()),
        );

        let err =
            send_tycode_runtime_session_settings_update(&mut runtime_settings, &update, &stdin_tx)
                .expect_err("live default_agent update should be rejected");

        assert!(err.contains("cannot be changed on a running session"));
        assert!(stdin_rx.try_recv().is_err());
        assert_eq!(
            runtime_settings.expect("runtime settings")["default_agent"],
            "tycode"
        );
    }

    #[test]
    fn tycode_runtime_session_settings_reject_profile_update() {
        let (stdin_tx, _stdin_rx) = mpsc::unbounded_channel::<TycodeStdinCommand>();
        let mut runtime_settings = Some(serde_json::json!({}));
        let mut update = SessionSettingsValues::default();
        update.0.insert(
            TYCODE_PROFILE_SETTING.to_string(),
            SessionSettingValue::String("work".to_string()),
        );

        let err =
            send_tycode_runtime_session_settings_update(&mut runtime_settings, &update, &stdin_tx)
                .expect_err("live profile update should be rejected");

        assert!(
            err.contains("profile cannot be changed on a running session"),
            "{err}"
        );
    }

    #[test]
    fn tycode_schema_profile_select_appears_only_with_named_profiles() {
        let home = TempDir::new().expect("tempdir");
        let _guard = TestTycodeSchemaGuard::set(home.path(), true);
        let tycode_dir = home.path().join(".tycode");
        fs::create_dir_all(&tycode_dir).expect("create tycode dir");
        fs::write(tycode_dir.join("settings.toml"), b"").expect("write settings");

        // Only the shared settings file: no profile field.
        let schema = tycode_session_settings_schema();
        assert_eq!(
            schema
                .fields
                .iter()
                .map(|field| field.key.as_str())
                .collect::<Vec<_>>(),
            vec!["default_agent"]
        );

        // A named profile file makes the Select appear, default first.
        let profiles_dir = tycode_dir.join("profiles");
        fs::create_dir_all(&profiles_dir).expect("create profiles dir");
        fs::write(profiles_dir.join("work.toml"), b"").expect("write profile");
        let schema = tycode_session_settings_schema();
        let field = schema
            .fields
            .iter()
            .find(|field| field.key == TYCODE_PROFILE_SETTING)
            .expect("profile session field");
        assert!(!field.use_slider);
        match &field.field_type {
            SessionSettingFieldType::Select {
                options,
                default,
                nullable,
            } => {
                assert_eq!(default.as_deref(), Some("default"));
                assert!(!nullable);
                assert_eq!(
                    options
                        .iter()
                        .map(|option| option.value.as_str())
                        .collect::<Vec<_>>(),
                    vec!["default", "work"]
                );
            }
            other => panic!("profile should be Select, got {other:?}"),
        }
    }

    #[test]
    fn tycode_resolves_session_profile_and_refuses_unknown_names() {
        let home = TempDir::new().expect("tempdir");
        let _guard = TestTycodeSchemaGuard::set(home.path(), true);
        let tycode_dir = home.path().join(".tycode");
        let profiles_dir = tycode_dir.join("profiles");
        fs::create_dir_all(&profiles_dir).expect("create profiles dir");
        fs::write(profiles_dir.join("work.toml"), b"").expect("write profile");

        let mut settings = SessionSettingsValues::default();
        let profile = resolve_session_profile(&settings).expect("default profile");
        assert_eq!(profile.name, "default");
        assert_eq!(profile.settings_path, tycode_dir.join("settings.toml"));

        settings.0.insert(
            TYCODE_PROFILE_SETTING.to_string(),
            SessionSettingValue::String("work".to_string()),
        );
        let profile = resolve_session_profile(&settings).expect("named profile");
        assert_eq!(profile.settings_path, profiles_dir.join("work.toml"));

        settings.0.insert(
            TYCODE_PROFILE_SETTING.to_string(),
            SessionSettingValue::String("missing".to_string()),
        );
        let error = resolve_session_profile(&settings).expect_err("unknown profile");
        assert!(error.contains("does not exist"), "{error}");
    }

    #[tokio::test]
    async fn tycode_session_command_targets_the_selected_profile_file() {
        let dir = TempDir::new().expect("tempdir");
        let fake = write_fake_tycode_subprocess(dir.path(), &serde_json::json!({}));
        let _guard = TestTycodeSubprocessGuard::set(fake);
        let home = tycode_home_dir().expect("test home");
        let profiles_dir = home.join("profiles");
        fs::create_dir_all(&profiles_dir).expect("create profiles dir");
        fs::write(profiles_dir.join("work.toml"), b"").expect("write profile");

        let settings_path_argument = |command: &Command| {
            let arguments = command
                .as_std()
                .get_args()
                .map(|argument| argument.to_string_lossy().to_string())
                .collect::<Vec<_>>();
            let index = arguments
                .iter()
                .position(|argument| argument == "--settings-path")
                .expect("settings path argument");
            arguments[index + 1].clone()
        };

        let config = BackendSpawnConfig::default();
        let command = tycode_session_command(TycodeCommandPurpose::NewSession, &config, "[]")
            .await
            .expect("default profile command");
        assert_eq!(
            settings_path_argument(&command),
            home.join("settings.toml").to_string_lossy()
        );

        let mut config = BackendSpawnConfig {
            session_settings: Some(SessionSettingsValues::default()),
            ..Default::default()
        };
        config
            .session_settings
            .as_mut()
            .expect("session settings")
            .0
            .insert(
                TYCODE_PROFILE_SETTING.to_string(),
                SessionSettingValue::String("work".to_string()),
            );
        let command = tycode_session_command(TycodeCommandPurpose::NewSession, &config, "[]")
            .await
            .expect("named profile command");
        assert_eq!(
            settings_path_argument(&command),
            profiles_dir.join("work.toml").to_string_lossy()
        );

        config
            .session_settings
            .as_mut()
            .expect("session settings")
            .0
            .insert(
                TYCODE_PROFILE_SETTING.to_string(),
                SessionSettingValue::String("missing".to_string()),
            );
        let error = tycode_session_command(TycodeCommandPurpose::NewSession, &config, "[]")
            .await
            .expect_err("unknown profile must fail the launch");
        assert!(error.contains("does not exist"), "{error}");
    }

    #[tokio::test]
    async fn tycode_persist_refuses_stale_and_baseless_saves() {
        let dir = TempDir::new().expect("tempdir");
        let settings = serde_json::json!({"model_quality": "high"});
        let fake = write_fake_tycode_subprocess(dir.path(), &settings);
        let _guard = TestTycodeSubprocessGuard::set(fake);

        // A changed profile without its base is refused.
        let missing_base = serde_json::json!({
            "version": 1,
            "profiles": [{
                "name": "default",
                "settings_path": "",
                "settings": {"model_quality": "low"},
            }],
        });
        let error = persist_native_settings(missing_base)
            .await
            .expect_err("baseless save");
        assert!(error.contains("missing its base settings"), "{error}");

        // A base that no longer matches the live settings is refused.
        let stale = serde_json::json!({
            "version": 1,
            "profiles": [{
                "name": "default",
                "settings_path": "",
                "settings": {"model_quality": "low"},
                "base_settings": {"model_quality": "medium"},
            }],
        });
        let error = persist_native_settings(stale)
            .await
            .expect_err("stale save");
        assert!(error.contains("changed since they were loaded"), "{error}");

        // An unsupported document version fails closed.
        let wrong_version = serde_json::json!({"version": 999, "profiles": []});
        let error = persist_native_settings(wrong_version)
            .await
            .expect_err("versioned save");
        assert!(
            error.contains("unsupported Tycode settings document version"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn tycode_persist_actions_create_and_delete_profiles() {
        let dir = TempDir::new().expect("tempdir");
        let settings = serde_json::json!({"model_quality": "high"});
        let fake = write_fake_tycode_subprocess(dir.path(), &settings);
        let _guard = TestTycodeSubprocessGuard::set(fake);
        let home = tycode_home_dir().expect("test home");
        fs::write(home.join("settings.toml"), b"model_quality = \"high\"\n")
            .expect("write shared settings");

        let create = serde_json::json!({
            "version": 1,
            "profiles": [],
            "actions": [{"kind": "create_profile", "name": "work"}],
        });
        persist_native_settings(create)
            .await
            .expect("create profile");
        assert_eq!(
            fs::read(home.join("profiles/work.toml")).expect("created profile"),
            b"model_quality = \"high\"\n"
        );

        let delete = serde_json::json!({
            "version": 1,
            "profiles": [],
            "actions": [{"kind": "delete_profile", "name": "work"}],
        });
        persist_native_settings(delete)
            .await
            .expect("delete profile");
        assert!(!home.join("profiles/work.toml").exists());
    }

    #[test]
    fn tycode_message_added_flat_token_usage_maps_to_typed_usage_scopes() {
        let events = map_tycode_value_to_chat_events(&tycode_message_added_with_flat_usage());

        assert_eq!(events.len(), 1);
        let ChatEvent::MessageAdded(message) = &events[0] else {
            panic!("expected MessageAdded, got {:?}", events[0]);
        };
        assert_flat_usage_mapped(message);
    }

    #[test]
    fn tycode_stream_end_flat_token_usage_maps_to_typed_usage_scopes() {
        let events = map_tycode_value_to_chat_events(&serde_json::json!({
            "kind": "StreamEnd",
            "data": {
                "message": tycode_assistant_message_with_flat_usage()
            }
        }));

        assert_eq!(events.len(), 1);
        let ChatEvent::StreamEnd(end) = &events[0] else {
            panic!("expected StreamEnd, got {:?}", events[0]);
        };
        assert_flat_usage_mapped(&end.message);
    }

    #[test]
    fn tycode_malformed_known_event_surfaces_error_message() {
        let events = map_tycode_value_to_chat_events(&serde_json::json!({
            "kind": "StreamEnd",
            "data": {
                "message": {
                    "timestamp": "not a number"
                }
            }
        }));

        assert_eq!(events.len(), 2);
        let ChatEvent::MessageAdded(message) = &events[0] else {
            panic!("expected visible error message, got {:?}", events[0]);
        };
        assert!(matches!(&message.sender, MessageSender::Error));
        assert!(
            message.content.contains("Malformed Tycode StreamEnd event"),
            "unexpected error content: {}",
            message.content
        );
        assert!(
            matches!(events[1], ChatEvent::StreamEnd(_)),
            "malformed StreamEnd should still close the stream, got {:?}",
            events[1]
        );
        let mut stream_state = TycodeStreamState {
            open: true,
            accumulated_text: "partial".to_string(),
            ..Default::default()
        };
        for event in &events {
            stream_state.update(event);
        }
        assert!(
            !stream_state.open,
            "malformed StreamEnd must not leave cursor active"
        );
    }

    #[test]
    fn tycode_typing_false_synthesizes_stream_end_before_idle() {
        let raw_events = [
            serde_json::json!({
                "kind": "TypingStatusChanged",
                "data": true
            }),
            serde_json::json!({
                "kind": "StreamStart",
                "data": {
                    "message_id": "m1",
                    "agent": "tycode",
                    "model": "ClaudeSonnet46"
                }
            }),
            serde_json::json!({
                "kind": "StreamDelta",
                "data": {
                    "message_id": "m1",
                    "text": "Acknowledged."
                }
            }),
            serde_json::json!({
                "kind": "TypingStatusChanged",
                "data": false
            }),
        ];
        let mut stream_state = TycodeStreamState::default();
        let mut emitted = Vec::new();
        for raw_event in raw_events {
            emitted.extend(tycode_events_with_synthesized_completion(
                map_tycode_value_to_chat_events(&raw_event),
                &mut stream_state,
            ));
        }

        assert_eq!(
            event_kinds(&emitted),
            vec![
                "TypingStatusChanged(true)",
                "StreamStart",
                "StreamDelta",
                "StreamEnd",
                "TypingStatusChanged(false)",
            ]
        );
        let ChatEvent::StreamEnd(end) = &emitted[3] else {
            panic!("expected synthesized StreamEnd, got {:?}", emitted[3]);
        };
        assert_eq!(end.message.content, "Acknowledged.");
        assert!(matches!(
            &end.message.sender,
            MessageSender::Assistant { agent } if agent == "tycode"
        ));
        assert!(!stream_state.open);
    }

    /// Pinned to the tycode-subprocess 0.10.0 wire, captured live on
    /// 2026-07-22: `StreamEnd` carries no message id anywhere (not top-level,
    /// not inside `message`), and `TypingStatusChanged(false)` arrives after
    /// the real `StreamEnd`. The adapter must stamp the open stream's id onto
    /// the id-less end so Tyde's stream identity validators accept the turn;
    /// before this injection the end was rejected as MissingMessageId and the
    /// stuck active stream poisoned every later turn into a violation cascade.
    #[test]
    fn tycode_real_wire_id_less_stream_end_adopts_open_stream_identity() {
        let turn = |message_id: &str, word: &str| {
            [
                serde_json::json!({ "kind": "TypingStatusChanged", "data": true }),
                serde_json::json!({
                    "kind": "StreamStart",
                    "data": {
                        "message_id": message_id,
                        "agent": "tycode",
                        "model": "qwen-plus",
                        "model_version": "qwen-3.6-plus"
                    }
                }),
                serde_json::json!({
                    "kind": "StreamReasoningDelta",
                    "data": { "message_id": message_id, "text": "thinking" }
                }),
                serde_json::json!({
                    "kind": "StreamDelta",
                    "data": { "message_id": message_id, "text": word }
                }),
                serde_json::json!({
                    "kind": "StreamEnd",
                    "data": {
                        "message": {
                            "timestamp": 1784723793481_u64,
                            "sender": { "Assistant": { "agent": "tycode" } },
                            "content": word,
                            "reasoning": {
                                "text": "thinking",
                                "signature": null,
                                "blob": null,
                                "raw_json": null
                            },
                            "tool_calls": [],
                            "model_info": { "model": "qwen-plus", "version": "qwen-3.6-plus" },
                            "token_usage": {
                                "input_tokens": 7405,
                                "output_tokens": 40,
                                "total_tokens": 7445,
                                "cached_prompt_tokens": 0,
                                "cache_creation_input_tokens": null,
                                "reasoning_tokens": 34
                            },
                            "images": []
                        }
                    }
                }),
                serde_json::json!({ "kind": "TypingStatusChanged", "data": false }),
            ]
        };

        let mut stream_state = TycodeStreamState::default();
        let mut emitted = Vec::new();
        for raw_event in turn("msg-1784723792531", "pong")
            .into_iter()
            .chain(turn("msg-1784723904489", "marco"))
        {
            emitted.extend(tycode_events_with_synthesized_completion(
                map_tycode_value_to_chat_events(&raw_event),
                &mut stream_state,
            ));
        }

        let expected_ids = ["msg-1784723792531", "msg-1784723904489"];
        let mut ends = 0;
        for event in &emitted {
            match event {
                ChatEvent::StreamStart(start) => {
                    assert_eq!(start.message_id.as_deref(), Some(expected_ids[ends]));
                }
                ChatEvent::StreamDelta(delta) | ChatEvent::StreamReasoningDelta(delta) => {
                    assert_eq!(delta.message_id.as_deref(), Some(expected_ids[ends]));
                }
                ChatEvent::StreamEnd(end) => {
                    assert_eq!(
                        end.message.message_id.as_ref().map(|id| id.0.as_str()),
                        Some(expected_ids[ends]),
                        "id-less wire StreamEnd must adopt the open stream's id"
                    );
                    assert!(end.message.token_usage.is_some());
                    ends += 1;
                }
                _ => {}
            }
        }
        assert_eq!(ends, 2, "both real StreamEnds must survive unduplicated");
        assert!(!stream_state.open);
    }

    #[test]
    fn tycode_late_real_stream_end_updates_synthetic_completion_metadata() {
        let raw_events = [
            serde_json::json!({
                "kind": "StreamStart",
                "data": {
                    "message_id": "m1",
                    "agent": "tycode",
                    "model": "stream-model"
                }
            }),
            serde_json::json!({
                "kind": "StreamDelta",
                "data": {
                    "message_id": "m1",
                    "text": "Authoritative content."
                }
            }),
            serde_json::json!({
                "kind": "TypingStatusChanged",
                "data": false
            }),
            serde_json::json!({
                "kind": "StreamEnd",
                "data": {
                    "message": {
                        "message_id": "m1",
                        "timestamp": 1776827246365_u64,
                        "sender": {
                            "Assistant": {
                                "agent": "tycode"
                            }
                        },
                        "content": "Authoritative content.",
                        "reasoning": null,
                        "tool_calls": [],
                        "model_info": {
                            "model": "authoritative-model"
                        },
                        "token_usage": {
                            "input_tokens": 11,
                            "output_tokens": 7,
                            "total_tokens": 18
                        },
                        "context_breakdown": {
                            "system_prompt_bytes": 101,
                            "tool_io_bytes": 102,
                            "conversation_history_bytes": 103,
                            "reasoning_bytes": 104,
                            "context_injection_bytes": 105,
                            "input_tokens": 11,
                            "context_window": 200000
                        },
                        "images": []
                    }
                }
            }),
        ];
        let mut stream_state = TycodeStreamState::default();
        let mut emitted = Vec::new();
        for raw_event in raw_events {
            emitted.extend(tycode_events_with_synthesized_completion(
                map_tycode_value_to_chat_events(&raw_event),
                &mut stream_state,
            ));
        }

        assert_eq!(
            event_kinds(&emitted),
            vec![
                "StreamStart",
                "StreamDelta",
                "StreamEnd",
                "TypingStatusChanged(false)",
                "MessageMetadataUpdated",
            ]
        );
        assert_eq!(
            emitted
                .iter()
                .filter(|event| matches!(event, ChatEvent::StreamEnd(_)))
                .count(),
            1,
            "delayed real StreamEnd must not create a duplicate visible message"
        );
        let ChatEvent::StreamEnd(end) = &emitted[2] else {
            panic!("expected synthesized StreamEnd, got {:?}", emitted[2]);
        };
        assert_eq!(
            end.message.message_id,
            Some(ChatMessageId("m1".to_string()))
        );
        assert_eq!(end.message.content, "Authoritative content.");

        let ChatEvent::MessageMetadataUpdated(update) = &emitted[4] else {
            panic!("expected late metadata update, got {:?}", emitted[4]);
        };
        assert_eq!(update.message_id, ChatMessageId("m1".to_string()));
        assert_eq!(
            update.model_info.as_ref().map(|model| model.model.as_str()),
            Some("authoritative-model")
        );
        let usage = update
            .token_usage
            .as_ref()
            .expect("late real StreamEnd token usage should be preserved");
        for scope in [&usage.request, &usage.turn] {
            let TokenUsageScope::Known { usage } = scope else {
                panic!("request and turn usage should be known, got {scope:?}");
            };
            assert_eq!(usage.input_tokens, 11);
            assert_eq!(usage.output_tokens, 7);
            assert_eq!(usage.total_tokens, 18);
        }
        let context = update
            .context_breakdown
            .as_ref()
            .expect("late real StreamEnd context should be preserved");
        assert_eq!(context.system_prompt_bytes, 101);
        assert_eq!(context.tool_io_bytes, 102);
        assert_eq!(context.conversation_history_bytes, 103);
        assert_eq!(context.reasoning_bytes, 104);
        assert_eq!(context.context_injection_bytes, 105);
        assert_eq!(context.input_tokens, 11);
        assert_eq!(context.context_window, 200000);
    }

    #[test]
    fn tycode_real_stream_end_prevents_typing_false_synthesis() {
        let mut stream_state = TycodeStreamState::default();
        let emitted = tycode_events_with_synthesized_completion(
            vec![
                ChatEvent::TypingStatusChanged(true),
                ChatEvent::StreamStart(protocol::StreamStartData {
                    message_id: Some("m1".to_string()),
                    agent: "tycode".to_string(),
                    model: Some("ClaudeSonnet46".to_string()),
                }),
                ChatEvent::StreamDelta(StreamTextDeltaData {
                    message_id: Some("m1".to_string()),
                    text: "Acknowledged.".to_string(),
                }),
                ChatEvent::StreamEnd(StreamEndData {
                    message: tycode_assistant_chat_message("Real end."),
                }),
                ChatEvent::TypingStatusChanged(false),
            ],
            &mut stream_state,
        );

        assert_eq!(
            emitted
                .iter()
                .filter(|event| matches!(event, ChatEvent::StreamEnd(_)))
                .count(),
            1
        );
        let ChatEvent::StreamEnd(end) = &emitted[3] else {
            panic!("expected real StreamEnd, got {:?}", emitted[3]);
        };
        assert_eq!(end.message.content, "Real end.");
        assert_eq!(event_kinds(&emitted)[4], "TypingStatusChanged(false)");
    }

    #[test]
    fn tycode_diagnostics_redact_settings_payloads_but_keep_event_kind() {
        let diagnostic = tycode_line_diagnostic(
            r#"{"kind":"Settings","data":{"providers":{"openrouter":{"api_key":"secret"}}}}"#,
        );

        assert_eq!(diagnostic, r#"{"kind":"Settings","data":"<redacted>"}"#);
    }

    #[test]
    fn tycode_stderr_diagnostics_preserve_sanitized_context() {
        let diagnostic =
            tycode_text_diagnostic("Fatal provider setup failed: api_key sk-secret-value invalid");

        assert!(diagnostic.contains("Fatal provider setup failed"));
        assert!(diagnostic.contains("api_key"));
        assert!(!diagnostic.contains("sk-secret-value"));
    }

    fn tycode_message_added_with_flat_usage() -> Value {
        serde_json::json!({
            "kind": "MessageAdded",
            "data": tycode_assistant_message_with_flat_usage()
        })
    }

    fn tycode_assistant_message_with_flat_usage() -> Value {
        serde_json::json!({
            "timestamp": 123,
            "sender": {
                "Assistant": {
                    "agent": "tycode"
                }
            },
            "content": "done",
            "reasoning": null,
            "tool_calls": [],
            "model_info": {
                "model": "claude-fable",
                "version": "claude-fable-5"
            },
            "token_usage": {
                "input_tokens": 11,
                "output_tokens": 7,
                "total_tokens": 18,
                "cached_prompt_tokens": 3,
                "cache_creation_input_tokens": 5,
                "reasoning_tokens": 2
            },
            "context_breakdown": null,
            "images": []
        })
    }

    fn assert_flat_usage_mapped(message: &ChatMessage) {
        let usage = message
            .token_usage
            .as_ref()
            .expect("flat usage should map to MessageTokenUsage");
        for scope in [&usage.request, &usage.turn] {
            let TokenUsageScope::Known { usage } = scope else {
                panic!("request and turn usage should be known, got {scope:?}");
            };
            assert_eq!(usage.input_tokens, 11);
            assert_eq!(usage.output_tokens, 7);
            assert_eq!(usage.total_tokens, 18);
            assert_eq!(usage.cached_prompt_tokens, Some(3));
            assert_eq!(usage.cache_creation_input_tokens, Some(5));
            assert_eq!(usage.reasoning_tokens, Some(2));
        }
        assert_eq!(
            usage.cumulative,
            TokenUsageScope::Unavailable {
                reason: TokenUsageUnavailableReason::BackendDidNotReport
            }
        );
    }

    fn event_kinds(events: &[ChatEvent]) -> Vec<&'static str> {
        events
            .iter()
            .map(|event| match event {
                ChatEvent::TypingStatusChanged(true) => "TypingStatusChanged(true)",
                ChatEvent::TypingStatusChanged(false) => "TypingStatusChanged(false)",
                ChatEvent::StreamStart(_) => "StreamStart",
                ChatEvent::StreamDelta(_) => "StreamDelta",
                ChatEvent::StreamEnd(_) => "StreamEnd",
                ChatEvent::MessageAdded(_) => "MessageAdded",
                ChatEvent::MessageMetadataUpdated(_) => "MessageMetadataUpdated",
                ChatEvent::StreamReasoningDelta(_) => "StreamReasoningDelta",
                ChatEvent::ToolRequest(_) => "ToolRequest",
                ChatEvent::ToolProgress(_) => "ToolProgress",
                ChatEvent::ToolExecutionCompleted(_) => "ToolExecutionCompleted",
                ChatEvent::TaskUpdate(_) => "TaskUpdate",
                ChatEvent::OperationCancelled(_) => "OperationCancelled",
                ChatEvent::RetryAttempt(_) => "RetryAttempt",
                ChatEvent::Orchestration(_) => "Orchestration",
            })
            .collect()
    }

    fn tycode_assistant_chat_message(content: &str) -> ChatMessage {
        ChatMessage {
            message_id: None,
            timestamp: 123,
            sender: MessageSender::Assistant {
                agent: "tycode".to_string(),
            },
            content: content.to_string(),
            reasoning: None,
            tool_calls: Vec::new(),
            model_info: None,
            token_usage: None,
            context_breakdown: None,
            images: None,
        }
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

        let err = apply_tycode_backend_config_overlay(
            &settings,
            &config,
            TycodeSettingsOverlayMode::SessionRuntime,
        )
        .expect_err("missing provider");
        assert!(err.contains("Configured Tycode active_provider 'missing' is absent"));
        assert!(err.contains("available: default"));
    }

    #[test]
    fn tycode_settings_classifier_keeps_only_pre_session_sender_error_advisory() {
        let event = serde_json::json!({
            "kind": "MessageAdded",
            "data": {
                "sender": "Error",
                "content": "No AI provider is configured. Configure one now."
            }
        });

        assert!(matches!(
            classify_tycode_settings_event(
                TycodeSettingsOperationPhase::AwaitSessionStarted,
                &event
            ),
            TycodeSettingsEventClassification::CollectAdvisory(
                BackendNativeSettingsAdvisory::NoProviderConfigured { .. }
            )
        ));
        assert!(matches!(
            classify_tycode_settings_event(
                TycodeSettingsOperationPhase::AwaitSettingsSchema,
                &event
            ),
            TycodeSettingsEventClassification::Fatal(message)
                if message == "No AI provider is configured. Configure one now."
        ));
    }

    #[test]
    fn tycode_settings_classifier_keeps_structured_error_fatal_before_session() {
        let event = serde_json::json!({
            "kind": "Error",
            "data": "settings loader failed"
        });

        assert!(matches!(
            classify_tycode_settings_event(
                TycodeSettingsOperationPhase::AwaitSessionStarted,
                &event
            ),
            TycodeSettingsEventClassification::Fatal(message)
                if message == "settings loader failed"
        ));
    }

    #[test]
    fn tycode_raw_command_has_exactly_one_settings_path() {
        let command = raw_tycode_command(
            "/tmp/tycode-subprocess",
            Path::new("/tmp/tyde-settings.toml"),
            "[]",
        );
        let arguments = command
            .as_std()
            .get_args()
            .map(|argument| argument.to_string_lossy().to_string())
            .collect::<Vec<_>>();
        assert_eq!(
            arguments
                .iter()
                .filter(|argument| argument.as_str() == "--settings-path")
                .count(),
            1
        );
        assert_eq!(
            arguments.iter().map(String::as_str).collect::<Vec<_>>(),
            vec![
                "--settings-path",
                "/tmp/tyde-settings.toml",
                "--workspace-roots",
                "[]"
            ]
        );
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
        assert!(save["settings"].get("default_agent").is_none());
        assert!(
            save["settings"]
                .get("orchestration_progress_messages")
                .is_none()
        );
        assert_eq!(save["settings"]["disable_streaming"], false);
        assert_eq!(save["settings"]["providers"], settings["providers"]);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fake_tycode_spawn_disables_supported_progress_messages_without_user_config() {
        let dir = TempDir::new().expect("tempdir");
        let settings = serde_json::json!({
            "active_provider": "default",
            "providers": {
                "default": { "type": "mock" }
            },
            "default_agent": "tycode",
            "orchestration_progress_messages": true
        });
        let fake = write_fake_tycode_subprocess(dir.path(), &settings);
        let log = dir.path().join("commands.jsonl");
        let _guard = TestTycodeSubprocessGuard::set(fake);

        let (backend, mut events) = TycodeBackend::spawn(
            Vec::new(),
            BackendSpawnConfig::default(),
            payload("hello Tycode"),
        )
        .await
        .expect("spawn fake Tycode");
        wait_for_fake_done(&mut events).await;
        backend.shutdown().await;

        let commands = read_fake_commands(&log);
        assert_eq!(commands.len(), 4, "commands: {commands:#?}");
        assert_eq!(commands[0], Value::String("GetSettings".to_string()));
        assert_eq!(commands[2], Value::String("GetSettings".to_string()));
        assert_eq!(
            commands[3],
            serde_json::json!({ "UserInput": "hello Tycode" })
        );

        let save = commands[1]
            .get("SaveSettings")
            .expect("SaveSettings command");
        assert_eq!(save["persist"], false);
        assert_eq!(save["settings"]["default_agent"], "tycode");
        assert_eq!(save["settings"]["orchestration_progress_messages"], false);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fake_tycode_spawn_tolerates_old_binary_that_drops_unknown_settings() {
        let dir = TempDir::new().expect("tempdir");
        let settings = serde_json::json!({
            "active_provider": "default",
            "providers": {
                "default": { "type": "mock" }
            },
            "model_quality": "high",
            "reasoning_effort": "Max",
            "autonomy_level": "plan_approval_required",
            "review_level": "None",
            "spawn_context_mode": "Fork"
        });
        let fake = write_fake_tycode_subprocess_dropping_unknown_settings(dir.path(), &settings);
        let log = dir.path().join("commands.jsonl");
        let _guard = TestTycodeSubprocessGuard::set(fake);

        let (backend, mut events) = TycodeBackend::spawn(
            Vec::new(),
            BackendSpawnConfig::default(),
            payload("hello old Tycode"),
        )
        .await
        .expect("spawn old fake Tycode");
        wait_for_fake_done(&mut events).await;
        backend.shutdown().await;

        let commands = read_fake_commands(&log);
        assert_eq!(commands.len(), 4, "commands: {commands:#?}");
        assert_eq!(commands[0], Value::String("GetSettings".to_string()));
        assert_eq!(commands[2], Value::String("GetSettings".to_string()));
        assert_eq!(
            commands[3],
            serde_json::json!({ "UserInput": "hello old Tycode" })
        );

        let save = commands[1]
            .get("SaveSettings")
            .expect("SaveSettings command");
        assert_eq!(save["persist"], false);
        assert!(
            save["settings"]
                .get("orchestration_progress_messages")
                .is_none()
        );
        assert!(save["settings"].get("default_agent").is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fake_tycode_spawn_sets_requested_root_agent_before_user_input() {
        let dir = TempDir::new().expect("tempdir");
        let settings = serde_json::json!({
            "active_provider": "default",
            "providers": {
                "default": { "type": "mock" }
            },
            "default_agent": "tycode",
            "orchestration_progress_messages": true
        });
        let fake = write_fake_tycode_subprocess(dir.path(), &settings);
        let log = dir.path().join("commands.jsonl");
        let _guard = TestTycodeSubprocessGuard::set_with_root_agent_support(fake);

        let mut session_settings = SessionSettingsValues::default();
        session_settings.0.insert(
            "default_agent".to_string(),
            SessionSettingValue::String("swarm".to_string()),
        );
        let config = BackendSpawnConfig {
            session_settings: Some(session_settings),
            ..Default::default()
        };

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
            serde_json::json!({ "SetRootAgent": { "agent": "swarm" } })
        );
        assert_eq!(
            commands[4],
            serde_json::json!({ "UserInput": "hello Tycode" })
        );

        let save = commands[1]
            .get("SaveSettings")
            .expect("SaveSettings command");
        assert_eq!(save["persist"], false);
        assert_eq!(save["settings"]["default_agent"], "tycode");
        assert_eq!(save["settings"]["orchestration_progress_messages"], false);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fake_tycode_spawn_surfaces_root_agent_rejection_before_prompt() {
        let dir = TempDir::new().expect("tempdir");
        let settings = serde_json::json!({
            "active_provider": "default",
            "providers": {
                "default": { "type": "mock" }
            },
            "default_agent": "tycode",
            "orchestration_progress_messages": true
        });
        let fake = write_fake_tycode_subprocess_rejecting_root_agent(dir.path(), &settings);
        let log = dir.path().join("commands.jsonl");
        let _guard = TestTycodeSubprocessGuard::set_with_root_agent_support(fake);

        let mut session_settings = SessionSettingsValues::default();
        session_settings.0.insert(
            "default_agent".to_string(),
            SessionSettingValue::String("swarm".to_string()),
        );
        let config = BackendSpawnConfig {
            session_settings: Some(session_settings),
            ..Default::default()
        };

        let err = match TycodeBackend::spawn(Vec::new(), config, payload("must not send")).await {
            Ok(_) => panic!("root agent rejection should fail startup"),
            Err(err) => err,
        };
        assert!(err.contains("Tycode SetRootAgent 'swarm' failed"));
        assert!(err.contains("Unknown agent type 'swarm'"));

        let commands = read_fake_commands(&log);
        assert_eq!(commands.len(), 4, "commands: {commands:#?}");
        assert_eq!(commands[0], Value::String("GetSettings".to_string()));
        assert_eq!(commands[2], Value::String("GetSettings".to_string()));
        assert_eq!(
            commands[3],
            serde_json::json!({ "SetRootAgent": { "agent": "swarm" } })
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fake_tycode_spawn_rejects_root_agent_when_command_is_unsupported() {
        let dir = TempDir::new().expect("tempdir");
        let settings = serde_json::json!({
            "active_provider": "default",
            "providers": {
                "default": { "type": "mock" }
            },
            "model_quality": "high",
            "reasoning_effort": "Max",
            "autonomy_level": "plan_approval_required",
            "review_level": "None",
            "spawn_context_mode": "Fork"
        });
        let fake = write_fake_tycode_subprocess_dropping_unknown_settings(dir.path(), &settings);
        let log = dir.path().join("commands.jsonl");
        let _guard = TestTycodeSubprocessGuard::set_without_root_agent_support(fake);

        let mut session_settings = SessionSettingsValues::default();
        session_settings.0.insert(
            "default_agent".to_string(),
            SessionSettingValue::String("swarm".to_string()),
        );
        let config = BackendSpawnConfig {
            session_settings: Some(session_settings),
            ..Default::default()
        };

        let err = match TycodeBackend::spawn(Vec::new(), config, payload("must not send")).await {
            Ok(_) => panic!("unsupported SetRootAgent should fail startup"),
            Err(err) => err,
        };
        assert!(err.contains("requires SetRootAgent support"));
        assert!(err.contains(TYCODE_VERSION));

        let commands = read_fake_commands(&log);
        assert_eq!(commands.len(), 3, "commands: {commands:#?}");
        assert_eq!(commands[0], Value::String("GetSettings".to_string()));
        assert_eq!(commands[2], Value::String("GetSettings".to_string()));
        assert!(
            commands
                .iter()
                .all(|command| command.get("SetRootAgent").is_none()),
            "unsupported Tycode must not receive SetRootAgent: {commands:#?}"
        );
        assert!(
            commands
                .iter()
                .all(|command| command.get("UserInput").is_none()),
            "prompt must not be sent after root-agent startup rejection: {commands:#?}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fake_tycode_spawn_rejects_root_agent_for_read_only_session() {
        let dir = TempDir::new().expect("tempdir");
        let settings = serde_json::json!({
            "active_provider": "default",
            "providers": {
                "default": { "type": "mock" }
            },
            "default_agent": "tycode",
            "orchestration_progress_messages": true
        });
        let fake = write_fake_tycode_subprocess(dir.path(), &settings);
        let log = dir.path().join("commands.jsonl");
        let _guard = TestTycodeSubprocessGuard::set_with_root_agent_support(fake);

        let mut session_settings = SessionSettingsValues::default();
        session_settings.0.insert(
            "default_agent".to_string(),
            SessionSettingValue::String("swarm".to_string()),
        );
        let config = BackendSpawnConfig {
            session_settings: Some(session_settings),
            resolved_spawn_config: crate::agent::customization::ResolvedSpawnConfig {
                access_mode: BackendAccessMode::ReadOnly,
                ..Default::default()
            },
            ..Default::default()
        };

        let err = match TycodeBackend::spawn(Vec::new(), config, payload("must not send")).await {
            Ok(_) => panic!("read-only root override should fail startup"),
            Err(err) => err,
        };
        assert!(err.contains("cannot be used with read-only Tycode sessions"));

        let commands = read_fake_commands(&log);
        assert_eq!(commands.len(), 3, "commands: {commands:#?}");
        assert_eq!(commands[0], Value::String("GetSettings".to_string()));
        assert_eq!(commands[2], Value::String("GetSettings".to_string()));
        assert!(
            commands
                .iter()
                .all(|command| command.get("SetRootAgent").is_none()),
            "read-only Tycode must not receive SetRootAgent: {commands:#?}"
        );
        assert!(
            commands
                .iter()
                .all(|command| command.get("UserInput").is_none()),
            "prompt must not be sent after read-only root-agent rejection: {commands:#?}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tycode_backend_config_persistent_save_uses_persist_true() {
        let dir = TempDir::new().expect("tempdir");
        let settings = serde_json::json!({
            "active_provider": "default",
            "providers": {
                "default": { "type": "mock" },
                "other": { "type": "openrouter", "api_key": "secret" }
            },
            "model_quality": "high",
            "reasoning_effort": "Max",
            "autonomy_level": "fully_autonomous",
            "review_level": "Task",
            "spawn_context_mode": "Fresh"
        });
        let fake = write_fake_tycode_subprocess(dir.path(), &settings);
        let log = dir.path().join("commands.jsonl");
        let _guard = TestTycodeSubprocessGuard::set(fake);

        let mut values = BackendConfigValues::default();
        values.0.insert(
            "active_provider".to_string(),
            SessionSettingValue::String("other".to_string()),
        );
        values.0.insert(
            "model_quality".to_string(),
            SessionSettingValue::String("low".to_string()),
        );

        persist_backend_config(values)
            .await
            .expect("persist Tycode backend config");

        let commands = read_fake_commands(&log);
        assert_eq!(commands.len(), 4, "commands: {commands:#?}");
        assert_eq!(commands[0], Value::String("GetSettingsSchema".to_string()));
        assert_eq!(commands[2], Value::String("GetSettingsSchema".to_string()));
        assert_eq!(commands[3], Value::String("GetSettingsSchema".to_string()));

        let save = commands[1]
            .get("SaveSettings")
            .expect("SaveSettings command");
        assert_eq!(save["persist"], true);
        assert_eq!(save["settings"]["active_provider"], "other");
        assert_eq!(save["settings"]["model_quality"], "low");
        assert_eq!(save["settings"]["reasoning_effort"], "Max");
        assert_eq!(save["settings"]["autonomy_level"], "fully_autonomous");
        assert_eq!(save["settings"]["review_level"], "Task");
        assert_eq!(save["settings"]["spawn_context_mode"], "Fresh");
        assert_eq!(save["settings"]["providers"], settings["providers"]);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tycode_native_settings_snapshot_carries_current_values_and_groups() {
        let dir = TempDir::new().expect("tempdir");
        let settings = serde_json::json!({
            "active_provider": "other",
            "providers": {
                "default": { "type": "mock" },
                "other": { "type": "openrouter", "api_key": "secret" }
            },
            "model_quality": "high",
            "reasoning_effort": "Max",
            "modules": {
                "memory": { "enabled": true }
            },
            "unrelated": { "keep": true }
        });
        let fake = write_fake_tycode_subprocess(dir.path(), &settings);
        let log = dir.path().join("commands.jsonl");
        let _guard = TestTycodeSubprocessGuard::set(fake);

        let snapshot = native_settings_snapshot().await;

        assert_eq!(snapshot.status, BackendConfigSnapshotStatus::Ready);
        let mut expected_settings = settings.clone();
        expected_settings["profile"] = Value::String("default".to_string());
        assert_eq!(snapshot.settings.as_ref(), Some(&expected_settings));
        assert!(
            snapshot
                .groups
                .iter()
                .any(|group| group.id == "providers" && group.settings_path.is_empty()),
            "providers group should be carried through: {:?}",
            snapshot.groups
        );
        assert!(
            snapshot.groups.iter().any(|group| {
                group.id == "module:memory"
                    && group.settings_path == vec!["modules".to_string(), "memory".to_string()]
            }),
            "module group should be carried through: {:?}",
            snapshot.groups
        );
        assert_eq!(
            read_fake_commands(&log),
            vec![Value::String("GetSettingsSchema".to_string())]
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tycode_native_settings_pre_session_error_is_ready_advisory() {
        let dir = TempDir::new().expect("tempdir");
        let settings = serde_json::json!({
            "active_provider": null,
            "providers": {}
        });
        let fake = write_fake_tycode_subprocess_with_pre_session_advisory(dir.path(), &settings);
        let _guard = TestTycodeSubprocessGuard::set(fake);

        let snapshot = native_settings_snapshot().await;

        assert_eq!(snapshot.status, BackendConfigSnapshotStatus::Ready);
        assert!(snapshot.settings.is_some());
        assert!(snapshot.advisories.iter().any(|advisory| matches!(
            advisory,
            BackendNativeSettingsAdvisory::NoProviderConfigured { message }
                if message == "No AI provider is configured. Configure one now."
        )));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tycode_native_settings_post_command_error_is_unavailable() {
        let dir = TempDir::new().expect("tempdir");
        let settings = serde_json::json!({
            "active_provider": null,
            "providers": {}
        });
        let fake = write_fake_tycode_subprocess_with_post_command_error(dir.path(), &settings);
        let _guard = TestTycodeSubprocessGuard::set(fake);

        let snapshot = native_settings_snapshot().await;

        assert_eq!(snapshot.status, BackendConfigSnapshotStatus::Unavailable);
        assert!(snapshot.settings.is_none());
        let message = snapshot.message.expect("fatal probe message");
        assert!(message.contains("native settings probe failed"));
        assert!(message.contains("waiting for SettingsSchema"));
        assert!(message.contains("schema command failed"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tycode_backend_config_persistent_save_empty_without_previous_is_noop() {
        let dir = TempDir::new().expect("tempdir");
        let settings = serde_json::json!({
            "active_provider": "other",
            "providers": {
                "default": { "type": "mock" },
                "other": { "type": "openrouter", "api_key": "secret" }
            },
            "model_quality": "high",
            "reasoning_effort": "Max",
            "autonomy_level": "fully_autonomous",
            "review_level": "Task",
            "spawn_context_mode": "Fresh"
        });
        let fake = write_fake_tycode_subprocess(dir.path(), &settings);
        let log = dir.path().join("commands.jsonl");
        let _guard = TestTycodeSubprocessGuard::set(fake);

        let incoming = BackendConfigValues::default();
        let previous = BackendConfigValues::default();
        let values = tycode_backend_config_persistence_values(&incoming, &previous);
        assert!(values.0.is_empty());

        persist_backend_config(values)
            .await
            .expect("persist empty Tycode backend config");
        assert!(
            !log.exists(),
            "empty config with no previous Tyde-managed keys should not spawn Tycode"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tycode_backend_config_snapshot_reads_current_settings_without_save() {
        let dir = TempDir::new().expect("tempdir");
        let settings = serde_json::json!({
            "active_provider": "other",
            "providers": {
                "default": { "type": "mock" },
                "other": { "type": "openrouter", "api_key": "secret" }
            },
            "model_quality": "high",
            "reasoning_effort": "Max",
            "autonomy_level": "fully_autonomous",
            "review_level": "Task",
            "spawn_context_mode": "Fresh"
        });
        let fake = write_fake_tycode_subprocess(dir.path(), &settings);
        let log = dir.path().join("commands.jsonl");
        let _guard = TestTycodeSubprocessGuard::set(fake);

        let values = backend_config_snapshot()
            .await
            .expect("read Tycode backend config snapshot");

        assert_eq!(
            values.0.get("active_provider"),
            Some(&SessionSettingValue::String("other".to_string()))
        );
        assert_eq!(
            values.0.get("model_quality"),
            Some(&SessionSettingValue::String("high".to_string()))
        );
        assert_eq!(
            values.0.get("reasoning_effort"),
            Some(&SessionSettingValue::String("Max".to_string()))
        );
        assert_eq!(
            read_fake_commands(&log),
            vec![Value::String("GetSettingsSchema".to_string())]
        );
    }

    #[test]
    fn tycode_backend_config_persistent_save_omitted_previous_key_is_preserved() {
        let mut incoming = BackendConfigValues::default();
        incoming.0.insert(
            "model_quality".to_string(),
            SessionSettingValue::String("low".to_string()),
        );
        let mut previous = BackendConfigValues::default();
        previous.0.insert(
            "active_provider".to_string(),
            SessionSettingValue::String("default".to_string()),
        );

        let values = tycode_backend_config_persistence_values(&incoming, &previous);

        assert_eq!(values.0.len(), 1);
        assert_eq!(
            values.0.get("model_quality"),
            Some(&SessionSettingValue::String("low".to_string()))
        );
        assert!(
            !values.0.contains_key("active_provider"),
            "omitted Tycode keys are preserved by generic settings merge, not reset during persistence"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tycode_backend_config_persistent_save_null_resets_only_that_key() {
        let dir = TempDir::new().expect("tempdir");
        let settings = serde_json::json!({
            "active_provider": "other",
            "providers": {
                "default": { "type": "mock" },
                "other": { "type": "openrouter", "api_key": "secret" }
            },
            "model_quality": "high",
            "reasoning_effort": "Max",
            "autonomy_level": "fully_autonomous",
            "review_level": "Task",
            "spawn_context_mode": "Fresh",
            "unmanaged_top_level": { "keep": true }
        });
        let fake = write_fake_tycode_subprocess(dir.path(), &settings);
        let log = dir.path().join("commands.jsonl");
        let _guard = TestTycodeSubprocessGuard::set(fake);

        let mut values = BackendConfigValues::default();
        values
            .0
            .insert("review_level".to_string(), SessionSettingValue::Null);

        persist_backend_config(values)
            .await
            .expect("persist Tycode backend config with explicit null");

        let commands = read_fake_commands(&log);
        assert_eq!(commands.len(), 4, "commands: {commands:#?}");
        assert_eq!(commands[0], Value::String("GetSettingsSchema".to_string()));
        assert_eq!(commands[2], Value::String("GetSettingsSchema".to_string()));
        assert_eq!(commands[3], Value::String("GetSettingsSchema".to_string()));

        let save = commands[1]
            .get("SaveSettings")
            .expect("SaveSettings command");
        assert_eq!(save["persist"], true);
        assert_eq!(save["settings"]["active_provider"], "other");
        assert_eq!(save["settings"]["model_quality"], "high");
        assert_eq!(save["settings"]["reasoning_effort"], "Max");
        assert_eq!(save["settings"]["autonomy_level"], "fully_autonomous");
        assert_eq!(save["settings"]["review_level"], "None");
        assert_eq!(save["settings"]["spawn_context_mode"], "Fresh");
        assert_eq!(save["settings"]["providers"], settings["providers"]);
        assert_eq!(
            save["settings"]["unmanaged_top_level"],
            serde_json::json!({ "keep": true })
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tycode_backend_config_persistent_save_empty_update_resets_previous_keys() {
        let dir = TempDir::new().expect("tempdir");
        let settings = serde_json::json!({
            "active_provider": "other",
            "providers": {
                "default": { "type": "mock" },
                "other": { "type": "openrouter", "api_key": "secret" }
            },
            "model_quality": "high",
            "reasoning_effort": "Max",
            "autonomy_level": "fully_autonomous",
            "review_level": "Task",
            "spawn_context_mode": "Fresh",
            "unmanaged_top_level": { "keep": true }
        });
        let fake = write_fake_tycode_subprocess(dir.path(), &settings);
        let log = dir.path().join("commands.jsonl");
        let _guard = TestTycodeSubprocessGuard::set(fake);

        let incoming = BackendConfigValues::default();
        let mut previous = BackendConfigValues::default();
        previous.0.insert(
            "model_quality".to_string(),
            SessionSettingValue::String("high".to_string()),
        );
        let values = tycode_backend_config_persistence_values(&incoming, &previous);
        assert_eq!(values.0.len(), 1);
        assert_eq!(
            values.0.get("model_quality"),
            Some(&SessionSettingValue::Null)
        );

        persist_backend_config(values)
            .await
            .expect("persist Tycode backend config with removed key");

        let commands = read_fake_commands(&log);
        assert_eq!(commands.len(), 4, "commands: {commands:#?}");
        assert_eq!(commands[0], Value::String("GetSettingsSchema".to_string()));
        assert_eq!(commands[2], Value::String("GetSettingsSchema".to_string()));
        assert_eq!(commands[3], Value::String("GetSettingsSchema".to_string()));

        let save = commands[1]
            .get("SaveSettings")
            .expect("SaveSettings command");
        assert_eq!(save["persist"], true);
        assert_eq!(save["settings"]["active_provider"], "other");
        assert_eq!(save["settings"]["model_quality"], Value::Null);
        assert_eq!(save["settings"]["reasoning_effort"], "Max");
        assert_eq!(save["settings"]["autonomy_level"], "fully_autonomous");
        assert_eq!(save["settings"]["review_level"], "Task");
        assert_eq!(save["settings"]["spawn_context_mode"], "Fresh");
        assert_eq!(save["settings"]["providers"], settings["providers"]);
        assert_eq!(
            save["settings"]["unmanaged_top_level"],
            serde_json::json!({ "keep": true })
        );
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

        let save = commands[1]
            .get("SaveSettings")
            .expect("SaveSettings command");
        assert!(save["settings"].get("default_agent").is_none());
        assert!(
            save["settings"]
                .get("orchestration_progress_messages")
                .is_none()
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
        previous_home_dir: Option<PathBuf>,
        previous_sessions_dir: Option<PathBuf>,
        previous_timeout: Option<Duration>,
        previous_set_root_agent_supported: Option<bool>,
    }

    static TEST_TYCODE_SUBPROCESS_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn write_test_shared_settings(home: &Path) {
        let directory = home.join(".tycode");
        fs::create_dir_all(&directory).expect("create test Tycode directory");
        let shared = directory.join("settings.toml");
        if !shared.exists() {
            fs::write(&shared, b"").expect("write test shared settings");
        }
    }

    struct TestTycodeRootAgentSupportGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        previous_set_root_agent_supported: Option<bool>,
    }

    impl TestTycodeRootAgentSupportGuard {
        fn set(supported: bool) -> Self {
            let lock = TEST_TYCODE_SUBPROCESS_MUTEX
                .lock()
                .expect("test Tycode subprocess mutex poisoned");
            let mut configured_set_root_agent_supported = TEST_TYCODE_SET_ROOT_AGENT_SUPPORTED
                .lock()
                .expect("test Tycode SetRootAgent support mutex poisoned");
            let previous_set_root_agent_supported =
                configured_set_root_agent_supported.replace(supported);
            drop(configured_set_root_agent_supported);
            Self {
                _lock: lock,
                previous_set_root_agent_supported,
            }
        }
    }

    impl Drop for TestTycodeRootAgentSupportGuard {
        fn drop(&mut self) {
            *TEST_TYCODE_SET_ROOT_AGENT_SUPPORTED
                .lock()
                .expect("test Tycode SetRootAgent support mutex poisoned") =
                self.previous_set_root_agent_supported.take();
        }
    }

    /// Pins both the Tycode home (profile discovery reads the filesystem) and
    /// the SetRootAgent support flag, so schema tests never observe the
    /// developer's real `~/.tycode`.
    struct TestTycodeSchemaGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        previous_home_dir: Option<PathBuf>,
        previous_set_root_agent_supported: Option<bool>,
    }

    impl TestTycodeSchemaGuard {
        fn set(home: &Path, root_agent_supported: bool) -> Self {
            let lock = TEST_TYCODE_SUBPROCESS_MUTEX
                .lock()
                .expect("test Tycode subprocess mutex poisoned");
            let previous_home_dir = TEST_TYCODE_HOME_DIR
                .lock()
                .expect("test Tycode home dir mutex poisoned")
                .replace(home.to_path_buf());
            let previous_set_root_agent_supported = TEST_TYCODE_SET_ROOT_AGENT_SUPPORTED
                .lock()
                .expect("test Tycode SetRootAgent support mutex poisoned")
                .replace(root_agent_supported);
            Self {
                _lock: lock,
                previous_home_dir,
                previous_set_root_agent_supported,
            }
        }
    }

    impl Drop for TestTycodeSchemaGuard {
        fn drop(&mut self) {
            *TEST_TYCODE_HOME_DIR
                .lock()
                .expect("test Tycode home dir mutex poisoned") = self.previous_home_dir.take();
            *TEST_TYCODE_SET_ROOT_AGENT_SUPPORTED
                .lock()
                .expect("test Tycode SetRootAgent support mutex poisoned") =
                self.previous_set_root_agent_supported.take();
        }
    }

    impl TestTycodeSubprocessGuard {
        fn set(path: String) -> Self {
            Self::set_with_options(path, None, None)
        }

        fn set_with_root_agent_support(path: String) -> Self {
            Self::set_with_options_inner(path, None, None, Some(true))
        }

        fn set_without_root_agent_support(path: String) -> Self {
            Self::set_with_options_inner(path, None, None, Some(false))
        }

        fn set_with_options(
            path: String,
            sessions_dir: Option<PathBuf>,
            startup_timeout: Option<Duration>,
        ) -> Self {
            Self::set_with_options_inner(path, sessions_dir, startup_timeout, None)
        }

        fn set_with_options_inner(
            path: String,
            sessions_dir: Option<PathBuf>,
            startup_timeout: Option<Duration>,
            set_root_agent_supported: Option<bool>,
        ) -> Self {
            let _ = crate::process_env::resolved_child_process_path();
            let lock = TEST_TYCODE_SUBPROCESS_MUTEX
                .lock()
                .expect("test Tycode subprocess mutex poisoned");
            let test_home = PathBuf::from(&path)
                .parent()
                .expect("fake Tycode subprocess parent")
                .join("tycode-home");
            let mut configured = TEST_TYCODE_SUBPROCESS_BIN
                .lock()
                .expect("test Tycode subprocess bin mutex poisoned");
            let previous_bin = configured.replace(path);
            drop(configured);
            let mut configured_home = TEST_TYCODE_HOME_DIR
                .lock()
                .expect("test Tycode home dir mutex poisoned");
            let previous_home_dir = configured_home.replace(test_home.clone());
            drop(configured_home);
            write_test_shared_settings(&test_home);
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
            let mut configured_set_root_agent_supported = TEST_TYCODE_SET_ROOT_AGENT_SUPPORTED
                .lock()
                .expect("test Tycode SetRootAgent support mutex poisoned");
            let previous_set_root_agent_supported = std::mem::replace(
                &mut *configured_set_root_agent_supported,
                set_root_agent_supported,
            );
            drop(configured_set_root_agent_supported);
            Self {
                _lock: lock,
                previous_bin,
                previous_home_dir,
                previous_sessions_dir,
                previous_timeout,
                previous_set_root_agent_supported,
            }
        }
    }

    impl Drop for TestTycodeSubprocessGuard {
        fn drop(&mut self) {
            *TEST_TYCODE_SUBPROCESS_BIN
                .lock()
                .expect("test Tycode subprocess bin mutex poisoned") = self.previous_bin.take();
            *TEST_TYCODE_HOME_DIR
                .lock()
                .expect("test Tycode home dir mutex poisoned") = self.previous_home_dir.take();
            *TEST_TYCODE_SESSIONS_DIR
                .lock()
                .expect("test Tycode sessions dir mutex poisoned") =
                self.previous_sessions_dir.take();
            *TEST_TYCODE_STARTUP_TIMEOUT
                .lock()
                .expect("test Tycode startup timeout mutex poisoned") =
                self.previous_timeout.take();
            *TEST_TYCODE_SET_ROOT_AGENT_SUPPORTED
                .lock()
                .expect("test Tycode SetRootAgent support mutex poisoned") =
                self.previous_set_root_agent_supported.take();
        }
    }

    fn write_fake_tycode_subprocess(dir: &Path, settings: &Value) -> String {
        write_fake_tycode_subprocess_with_options(dir, settings, false, false, false)
    }

    fn write_fake_tycode_subprocess_dropping_unknown_settings(
        dir: &Path,
        settings: &Value,
    ) -> String {
        write_fake_tycode_subprocess_with_options(dir, settings, true, false, false)
    }

    fn write_fake_tycode_subprocess_rejecting_root_agent(dir: &Path, settings: &Value) -> String {
        write_fake_tycode_subprocess_with_options(dir, settings, false, true, false)
    }

    fn write_fake_tycode_subprocess_canonicalizing_native_settings(
        dir: &Path,
        settings: &Value,
    ) -> String {
        write_fake_tycode_subprocess_with_options(dir, settings, false, false, true)
    }

    fn write_fake_tycode_subprocess_with_pre_session_advisory(
        dir: &Path,
        settings: &Value,
    ) -> String {
        let script = write_fake_tycode_subprocess(dir, settings);
        let body = fs::read_to_string(&script).expect("read fake Tycode script");
        let body = body.replacen(
            "emit({\"kind\": \"SessionStarted\", \"data\": {\"session_id\": \"fake-session\"}})",
            "emit(message(\"Error\", \"No AI provider is configured. Configure one now.\"))\nemit({\"kind\": \"SessionStarted\", \"data\": {\"session_id\": \"fake-session\"}})",
            1,
        );
        fs::write(&script, body).expect("rewrite fake Tycode advisory script");
        script
    }

    fn write_fake_tycode_subprocess_with_post_command_error(
        dir: &Path,
        settings: &Value,
    ) -> String {
        let script = write_fake_tycode_subprocess(dir, settings);
        let body = fs::read_to_string(&script).expect("read fake Tycode script");
        let body = body.replacen(
            "emit({\"kind\": \"SettingsSchema\", \"data\": {\"schema\": {\"settings\": settings, \"groups\": settings_groups}}})",
            "emit(message(\"Error\", \"schema command failed\"))",
            1,
        );
        fs::write(&script, body).expect("rewrite fake Tycode error script");
        script
    }

    fn write_fake_tycode_subprocess_with_options(
        dir: &Path,
        settings: &Value,
        drop_unknown_settings: bool,
        reject_root_agent: bool,
        canonicalize_native_settings: bool,
    ) -> String {
        let script = dir.join("fake_tycode_subprocess.py");
        let log = dir.join("commands.jsonl");
        let settings_literal =
            serde_json::to_string(&settings.to_string()).expect("settings literal");
        let mut default_settings = settings.clone();
        let default_settings_object = default_settings
            .as_object_mut()
            .expect("fake Tycode settings object");
        for key in TYCODE_MANAGED_SETTINGS {
            if default_settings_object.contains_key(*key) {
                default_settings_object
                    .insert((*key).to_string(), tycode_managed_setting_default(key));
            }
        }
        let default_settings_literal =
            serde_json::to_string(&default_settings.to_string()).expect("default settings literal");
        let log_literal = serde_json::to_string(&log.to_string_lossy()).expect("log literal");
        let drop_unknown_literal = if drop_unknown_settings {
            "True"
        } else {
            "False"
        };
        let reject_root_agent_literal = if reject_root_agent { "True" } else { "False" };
        let canonicalize_native_settings_literal = if canonicalize_native_settings {
            "True"
        } else {
            "False"
        };
        let body = r####"#!/usr/bin/env python3
import copy
import json
import sys

try:
    import tomllib
except ModuleNotFoundError:
    tomllib = None

def split_toml_top_level(value, delimiter):
    parts = []
    start = 0
    quote = None
    escaped = False
    square_depth = 0
    curly_depth = 0
    for index, character in enumerate(value):
        if quote is not None:
            if quote == '"' and escaped:
                escaped = False
            elif quote == '"' and character == "\\":
                escaped = True
            elif character == quote:
                quote = None
            continue
        if character in {'"', "'"}:
            quote = character
        elif character == "[":
            square_depth += 1
        elif character == "]":
            square_depth -= 1
        elif character == "{":
            curly_depth += 1
        elif character == "}":
            curly_depth -= 1
        elif character == delimiter and square_depth == 0 and curly_depth == 0:
            parts.append(value[start:index].strip())
            start = index + 1
    parts.append(value[start:].strip())
    return parts

def strip_toml_comment(line):
    quote = None
    escaped = False
    for index, character in enumerate(line):
        if quote is not None:
            if quote == '"' and escaped:
                escaped = False
            elif quote == '"' and character == "\\":
                escaped = True
            elif character == quote:
                quote = None
            continue
        if character in {'"', "'"}:
            quote = character
        elif character == "#":
            return line[:index]
    return line

def parse_toml_key(value):
    value = value.strip()
    if value.startswith('"'):
        return json.loads(value)
    if value.startswith("'") and value.endswith("'"):
        return value[1:-1]
    return value

def parse_toml_key_path(value):
    return [parse_toml_key(part) for part in split_toml_top_level(value, ".")]

def split_toml_assignment(line):
    parts = split_toml_top_level(line, "=")
    if len(parts) != 2:
        raise ValueError(f"unsupported TOML assignment: {line}")
    return parts

def assign_toml_path(target, path, value):
    current = target
    for part in path[:-1]:
        existing = current.setdefault(part, {})
        if not isinstance(existing, dict):
            raise ValueError(f"TOML key path collides at {part}")
        current = existing
    current[path[-1]] = value

def parse_toml_value(value):
    value = value.strip()
    if value.startswith('"'):
        return json.loads(value)
    if value.startswith("'") and value.endswith("'"):
        return value[1:-1]
    if value == "true":
        return True
    if value == "false":
        return False
    if value.startswith("[") and value.endswith("]"):
        contents = value[1:-1].strip()
        if not contents:
            return []
        return [
            parse_toml_value(item)
            for item in split_toml_top_level(contents, ",")
            if item
        ]
    if value.startswith("{") and value.endswith("}"):
        contents = value[1:-1].strip()
        result = {}
        if not contents:
            return result
        for entry in split_toml_top_level(contents, ","):
            key, item = split_toml_assignment(entry)
            assign_toml_path(result, parse_toml_key_path(key), parse_toml_value(item))
        return result
    number = value.replace("_", "")
    try:
        return json.loads(number)
    except json.JSONDecodeError:
        try:
            return int(number, 0)
        except ValueError as error:
            raise ValueError(f"unsupported TOML value: {value}") from error

def parse_toml(contents):
    result = {}
    current = result
    for raw_line in contents.splitlines():
        line = strip_toml_comment(raw_line).strip()
        if not line:
            continue
        if line.startswith("[") and line.endswith("]"):
            if line.startswith("[["):
                raise ValueError("TOML array tables are not used by the Tycode test fixture")
            current = result
            for part in parse_toml_key_path(line[1:-1]):
                nested = current.setdefault(part, {})
                if not isinstance(nested, dict):
                    raise ValueError(f"TOML table path collides at {part}")
                current = nested
            continue
        key, value = split_toml_assignment(line)
        assign_toml_path(current, parse_toml_key_path(key), parse_toml_value(value))
    return result

def load_toml(settings_file):
    if tomllib is not None:
        return tomllib.load(settings_file)
    return parse_toml(settings_file.read().decode("utf-8"))

initial_settings = json.loads(__SETTINGS__)
default_settings = json.loads(__DEFAULT_SETTINGS__)
known_settings_keys = set(default_settings.keys())
settings_path = None
for index, argument in enumerate(sys.argv):
    if argument == "--settings-path" and index + 1 < len(sys.argv):
        settings_path = sys.argv[index + 1]
        break
drop_unknown_settings = __DROP_UNKNOWN_SETTINGS__
reject_root_agent = __REJECT_ROOT_AGENT__
canonicalize_native_settings = __CANONICALIZE_NATIVE_SETTINGS__
log_path = __LOG__

def merge_defaults(base, loaded):
    merged = copy.deepcopy(base)
    for key, value in loaded.items():
        merged[key] = copy.deepcopy(value)
    return merged

def toml_value(value):
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, str):
        return json.dumps(value)
    if isinstance(value, (int, float)):
        return str(value)
    if isinstance(value, list):
        return "[" + ", ".join(toml_value(item) for item in value) + "]"
    if isinstance(value, dict):
        entries = [
            f"{json.dumps(key)} = {toml_value(item)}"
            for key, item in value.items()
            if item is not None
        ]
        return "{ " + ", ".join(entries) + " }"
    raise TypeError(f"unsupported TOML value: {type(value)}")

def write_toml_table(lines, prefix, table):
    if prefix:
        lines.append("[" + ".".join(json.dumps(part) for part in prefix) + "]")
    for key, value in table.items():
        if value is not None and not isinstance(value, dict):
            lines.append(f"{json.dumps(key)} = {toml_value(value)}")
    for key, value in table.items():
        if isinstance(value, dict):
            if lines and lines[-1] != "":
                lines.append("")
            write_toml_table(lines, prefix + [key], value)

def persist_toml(value):
    lines = []
    write_toml_table(lines, [], value)
    with open(settings_path, "w", encoding="utf-8") as settings_file:
        settings_file.write("\n".join(lines).rstrip() + "\n")

def is_empty_for_persistence(value):
    comparable = copy.deepcopy(value)
    default_agent = comparable.get("default_agent")
    if isinstance(default_agent, str) and not default_agent.strip():
        comparable["default_agent"] = default_settings.get("default_agent", "tycode")
    return comparable == default_settings

settings = copy.deepcopy(initial_settings)
if settings_path is not None:
    try:
        with open(settings_path, "rb") as settings_file:
            loaded_settings = load_toml(settings_file)
            if loaded_settings:
                settings = merge_defaults(default_settings, loaded_settings)
    except FileNotFoundError:
        settings = copy.deepcopy(default_settings)
        persist_toml(default_settings)
if drop_unknown_settings:
    settings = {key: value for key, value in settings.items() if key in known_settings_keys}
settings = dict(settings)
settings["profile"] = "default"

settings_groups = [
    {
        "id": "general",
        "title": "General",
        "kind": "core",
        "settings_path": [],
        "description": "General settings",
        "schema": {
            "type": "object",
            "properties": {
                "model_quality": {"type": ["string", "null"]},
                "reasoning_effort": {"type": ["string", "null"]},
            },
        },
    },
    {
        "id": "providers",
        "title": "Providers",
        "kind": "core",
        "settings_path": [],
        "description": "Provider settings",
        "schema": {
            "type": "object",
            "properties": {
                "active_provider": {"type": ["string", "null"]},
                "providers": {"type": "object"},
            },
        },
    },
    {
        "id": "module:memory",
        "title": "Memory",
        "kind": "module",
        "settings_path": ["modules", "memory"],
        "description": "Memory module settings",
        "schema": {
            "type": "object",
            "properties": {"enabled": {"type": "boolean"}},
        },
    },
]

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
    elif command == "GetSettingsSchema":
        emit({"kind": "SettingsSchema", "data": {"schema": {"settings": settings, "groups": settings_groups}}})
    elif isinstance(command, dict) and "SaveSettings" in command:
        incoming_settings = command["SaveSettings"]["settings"]
        if drop_unknown_settings:
            modeled_settings = {
                key: value
                for key, value in incoming_settings.items()
                if key in known_settings_keys
            }
        else:
            modeled_settings = incoming_settings
        settings = merge_defaults(default_settings, modeled_settings)
        settings = dict(settings)
        settings["profile"] = "default"
        if command["SaveSettings"].get("persist") and settings_path is not None:
            persisted = dict(settings)
            persisted.pop("profile", None)
            if is_empty_for_persistence(persisted):
                emit({"kind": "Error", "data": "Refusing to persist empty settings"})
                continue
            persist_toml(persisted)
    elif isinstance(command, dict) and "ChangeProvider" in command:
        emit(message("System", f"Switched to provider: {command['ChangeProvider']}"))
    elif isinstance(command, dict) and "SetRootAgent" in command:
        agent = command["SetRootAgent"]["agent"]
        valid_agents = {"one_shot", "tycode", "builder", "swarm"}
        if reject_root_agent or agent not in valid_agents:
            emit({
                "kind": "Error",
                "data": (
                    f"Unknown agent type '{agent}'. Available agents: "
                    "one_shot, tycode, builder, swarm"
                ),
            })
        else:
            emit({"kind": "RootAgentChanged", "data": {"agent": agent}})
    elif isinstance(command, dict) and "UserInput" in command:
        emit(message({"Assistant": {"agent": "tycode"}}, "fake done"))
"####
        .replace("__SETTINGS__", &settings_literal)
        .replace("__DEFAULT_SETTINGS__", &default_settings_literal)
        .replace("__DROP_UNKNOWN_SETTINGS__", drop_unknown_literal)
        .replace("__REJECT_ROOT_AGENT__", reject_root_agent_literal)
        .replace(
            "__CANONICALIZE_NATIVE_SETTINGS__",
            canonicalize_native_settings_literal,
        )
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

    #[cfg(unix)]
    fn write_fake_tycode_startup_stall_subprocess(dir: &Path) -> String {
        let script = dir.join("fake_tycode_startup_stall.sh");
        let body = r#"#!/bin/sh
while IFS= read -r line; do
  :
done
"#;
        std::fs::write(&script, body).expect("write fake Tycode startup stall script");
        make_executable(&script);
        script.to_string_lossy().to_string()
    }

    #[cfg(unix)]
    fn process_is_running(pid: &str) -> bool {
        std::process::Command::new("kill")
            .args(["-0", pid])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn dropping_tycode_startup_terminates_and_reaps_stalled_child() {
        let dir = TempDir::new().expect("tempdir");
        let fake = write_fake_tycode_startup_stall_subprocess(dir.path());
        let _guard = TestTycodeSubprocessGuard::set(fake);
        let (mut spawned_rx, reaped_rx) = install_tycode_startup_process_observer();
        let mut startup = Box::pin(TycodeBackend::spawn(
            vec![dir.path().to_string_lossy().to_string()],
            BackendSpawnConfig::default(),
            protocol::SendMessagePayload {
                message: "must not survive startup cancellation".to_owned(),
                images: None,
                origin: None,
                tool_response: None,
            },
        ));
        let pid = tokio::select! {
            biased;
            pid = &mut spawned_rx => {
                pid.expect("Tycode startup process observer must retain PID sender")
            }
            _ = startup.as_mut() => {
                panic!("Tycode startup completed before stall cancellation")
            }
        }
        .to_string();
        assert!(process_is_running(&pid), "fixture child must be running");

        drop(startup);

        reaped_rx
            .await
            .expect("Tycode startup worker must report reaping its cancelled child");
        assert!(
            !process_is_running(&pid),
            "closing the startup shutdown channel must terminate and reap the Tycode child"
        );
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn dropping_tycode_resume_startup_terminates_and_reaps_stalled_child() {
        let dir = TempDir::new().expect("tempdir");
        let fake = write_fake_tycode_startup_stall_subprocess(dir.path());
        let sessions_dir = dir.path().join("sessions");
        fs::create_dir_all(&sessions_dir).expect("create fake sessions dir");
        fs::write(
            sessions_dir.join("resume-cancelled-session.json"),
            serde_json::json!({
                "id": "resume-cancelled-session",
                "events": []
            })
            .to_string(),
        )
        .expect("write fake Tycode resume session");
        let _guard = TestTycodeSubprocessGuard::set_with_options(fake, Some(sessions_dir), None);
        let (mut spawned_rx, reaped_rx) = install_tycode_startup_process_observer();
        let mut startup = Box::pin(TycodeBackend::resume(
            vec![dir.path().to_string_lossy().to_string()],
            BackendSpawnConfig::default(),
            SessionId("resume-cancelled-session".to_owned()),
        ));
        let pid = tokio::select! {
            biased;
            pid = &mut spawned_rx => {
                pid.expect("Tycode resume process observer must retain PID sender")
            }
            _ = startup.as_mut() => {
                panic!("Tycode resume completed before stall cancellation")
            }
        }
        .to_string();
        assert!(
            process_is_running(&pid),
            "fixture resume child must be running"
        );

        drop(startup);

        reaped_rx
            .await
            .expect("Tycode resume worker must report reaping its cancelled child");
        assert!(
            !process_is_running(&pid),
            "closing the resume startup shutdown channel must terminate and reap the Tycode child"
        );
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
    fn map_tycode_value_to_chat_events_translates_orchestration() {
        let value = serde_json::json!({
            "kind": "Orchestration",
            "data": {
                "agent_id": "root-1",
                "agent_type": "swarm",
                "payload": {
                    "kind": "AgentStarted",
                    "parent_agent_id": null,
                    "task_preview": "plan the work",
                    "origin": { "kind": "Root" },
                    "depth": 1,
                    "interactive": true,
                    "model": "claude-fable"
                }
            }
        });

        let events = map_tycode_value_to_chat_events(&value);
        assert_eq!(events.len(), 1);
        match &events[0] {
            ChatEvent::Orchestration(event) => {
                assert_eq!(event.agent_id.0, "root-1");
                assert_eq!(event.agent_type.0, "swarm");
                match &event.payload {
                    OrchestrationPayload::AgentStarted {
                        parent_agent_id,
                        task_preview,
                        origin,
                        depth,
                        interactive,
                        model,
                    } => {
                        assert_eq!(parent_agent_id, &None);
                        assert_eq!(task_preview, "plan the work");
                        assert!(matches!(origin, OrchestrationAgentOrigin::Root));
                        assert_eq!(*depth, 1);
                        assert!(*interactive);
                        assert_eq!(model, &Some(protocol::TycodeModel::ClaudeFable));
                    }
                    other => panic!("expected AgentStarted, got {other:?}"),
                }
            }
            other => panic!("expected Orchestration event, got {other:?}"),
        }
    }

    #[test]
    fn map_tycode_value_to_chat_events_ignores_unknown_orchestration_payload_kind() {
        let value = serde_json::json!({
            "kind": "Orchestration",
            "data": {
                "agent_id": "root-1",
                "agent_type": "swarm",
                "payload": {
                    "kind": "FuturePayload",
                    "new_field": true
                }
            }
        });

        let events = map_tycode_value_to_chat_events(&value);
        assert!(events.is_empty());
    }

    #[test]
    fn map_tycode_value_to_chat_events_surfaces_malformed_known_orchestration_payload() {
        let value = serde_json::json!({
            "kind": "Orchestration",
            "data": {
                "agent_id": "root-1",
                "agent_type": "swarm",
                "payload": {
                    "kind": "AgentCompleted",
                    "status": "Succeeded"
                }
            }
        });

        let events = map_tycode_value_to_chat_events(&value);
        assert_eq!(events.len(), 1);
        match &events[0] {
            ChatEvent::MessageAdded(message) => {
                assert!(matches!(message.sender, MessageSender::Error));
                assert!(
                    message
                        .content
                        .contains("Malformed Tycode Orchestration event")
                );
                assert!(message.content.contains("AgentCompleted"));
            }
            other => panic!("expected visible error message, got {other:?}"),
        }
    }

    #[test]
    fn map_tycode_operation_cancelled_passes_through_without_terminal_worker_inference() {
        let value = serde_json::json!({
            "kind": "OperationCancelled",
            "data": {
                "message": "cancelled"
            }
        });

        let events = map_tycode_value_to_chat_events(&value);
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            ChatEvent::OperationCancelled(data) if data.message == "cancelled"
        ));
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
