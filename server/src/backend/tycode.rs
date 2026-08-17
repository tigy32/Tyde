use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use command_group::AsyncCommandGroup;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

use protocol::tycode_config::{
    TYCODE_NATIVE_SETTINGS_VERSION, TycodeNativeSettingsDoc, TycodeProfileSettings,
};
use protocol::{
    AgentInput, BackendConfigSnapshotStatus, BackendConfigValues, BackendKind,
    BackendNativeSettingsAdvisory, BackendNativeSettingsGroup, BackendNativeSettingsSnapshot,
    ChatEvent, ChatMessage, ChatMessageId, MessageMetadataUpdateData, MessageSender, ModelInfo,
    OrchestrationEvent, ReasoningData, SelectOption, SessionId, SessionSettingField,
    SessionSettingFieldType, SessionSettingValue, SessionSettingsSchema, SessionSettingsValues,
    StreamEndData, StreamTextDeltaData, ToolExecutionCompletedData, ToolRequest, ToolRequestType,
};

use super::{
    Backend, BackendCompactionCapability, BackendCompactionNotDispatchedReason,
    BackendCompactionRequest, BackendCompactionStart, BackendCompactionUnavailableReason,
    BackendSession, BackendSpawnConfig, BackendStartupError, EventStream, StartupMcpServer,
    StartupMcpTransport, apply_session_settings_update, backend_fork_unsupported_message,
    render_combined_spawn_instructions,
    setup::{TYCODE_VERSION, ensure_tycode_command_compatible, resolve_tycode_binary_path},
};
use crate::agent::customization::SkillSelection;
use crate::backend::agent_control_progress::{
    PendingToolNormalizationFailure, normalize_tyde_chat_event,
};
use crate::backend::skill_projection::{
    DescriptionPolicy, ProjectedSkill, ProjectionPolicy, SkillRefusal, create_private_dir,
    discard_wrapper, inspect_skill, write_wrapper,
};
use crate::backend::tycode_config;
use crate::process_env;

async fn subprocess_bin() -> Result<String, String> {
    let path =
        resolve_tycode_binary_path().ok_or_else(|| "tycode-subprocess not found".to_string())?;
    ensure_tycode_command_compatible(&path).await
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

/// Make sure the Tycode home directory exists before pointing the subprocess
/// at a settings file inside it. On a fresh machine Tycode itself creates its
/// defaults file on first run, but only if the directory exists for it to
/// write into.
fn ensure_tycode_home_dir(home: &Path) -> Result<(), String> {
    fs::create_dir_all(home)
        .map_err(|err| format!("Failed to create the Tycode home {}: {err}", home.display()))
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
    ensure_tycode_home_dir(&home)?;
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
/// Which reply the settings conversation is currently waiting for.
enum TycodeSettingsOperationPhase {
    SessionStarted,
    SettingsSchema,
    SettingsSaved,
}

impl TycodeSettingsOperationPhase {
    fn description(self) -> &'static str {
        match self {
            Self::SessionStarted => "waiting for SessionStarted",
            Self::SettingsSchema => "waiting for SettingsSchema",
            Self::SettingsSaved => "waiting for SettingsSchema after SaveSettings",
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
        return if phase == TycodeSettingsOperationPhase::SessionStarted {
            TycodeSettingsEventClassification::CollectAdvisory(tycode_settings_advisory(error))
        } else {
            TycodeSettingsEventClassification::Fatal(tycode_text_diagnostic(error))
        };
    }
    if tycode_session_started(value).is_some() {
        return if phase == TycodeSettingsOperationPhase::SessionStarted {
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
        return if phase == TycodeSettingsOperationPhase::SessionStarted {
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
    let mut phase = TycodeSettingsOperationPhase::SessionStarted;
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
                    phase = TycodeSettingsOperationPhase::SettingsSchema;
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
                    phase = TycodeSettingsOperationPhase::SettingsSaved;
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

#[derive(Debug)]
struct TempWorkspaceRoot {
    path: PathBuf,
}

impl TempWorkspaceRoot {
    fn new(prefix: &str) -> Result<Self, String> {
        let path = std::env::temp_dir().join(format!("{prefix}-{}", uuid::Uuid::new_v4()));
        // Private, not merely fresh: this root holds the session's steering and
        // the full text of every skill it projected, and a world-readable copy
        // of a user's skill instructions is not something to create silently.
        create_private_dir(&path)?;
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

/// How a selected skill is projected for Tycode.
///
/// `.tycode` is refused because the projection root *is* a workspace root: a
/// skill shipping its own `.tycode` directory would land beside the one Tyde
/// writes and could redirect the settings, steering, and skill tree Tyde owns.
///
/// A description is mandatory. Tycode's parser refuses a `SKILL.md` whose
/// frontmatter has no non-empty `description` and only logs a warning, so
/// without a synthesized fallback the skill would disappear from the catalog
/// with nothing to tell the user why.
const TYCODE_PROJECTION: ProjectionPolicy = ProjectionPolicy {
    refused_resource_names: &[".tycode", ".claude"],
    description: DescriptionPolicy::Required,
};

#[derive(Debug)]
struct TycodeCustomization {
    root: TempWorkspaceRoot,
    /// Names a Default session started without. `None` when nothing was
    /// dropped.
    degraded_notice: Option<String>,
}

/// Materialize this session's steering and skills into a temporary workspace
/// root that Tycode discovers for itself.
///
/// Tycode scans `<workspace root>/.tycode/skills/<name>/SKILL.md`, lists each
/// skill's name and description in its system prompt, and loads a body only when
/// the model calls `invoke_skill`. So skills are projected, never inlined: the
/// session pays one catalog line per skill instead of every body up front.
///
/// **A skill that cannot be projected never stops the session.** It costs the
/// session that one capability; refusing to start would cost it every other
/// skill and the workspace too. This holds for an explicit selection as much as
/// for the Default agent — but it is never silent: the notice names every
/// omitted skill and why, and the overlay is built solely from what was
/// actually projected, so the model is never told about a skill that is not
/// there.
fn materialize_tycode_customization(
    config: &BackendSpawnConfig,
) -> Result<Option<TycodeCustomization>, String> {
    let selected = &config.resolved_spawn_config.skills;
    let selection = config.resolved_spawn_config.skill_selection;
    let base_steering = render_combined_spawn_instructions(&config.resolved_spawn_config);
    if base_steering.is_none() && selected.is_empty() {
        return Ok(None);
    }
    let root = TempWorkspaceRoot::new("tyde-tycode-customization")?;

    // Inspect everything first, so a per-skill problem is a refusal the
    // selection policy can weigh rather than a half-built root.
    let mut refusals = Vec::new();
    let mut inspected = Vec::new();
    let mut claimed = BTreeSet::new();
    for skill in selected {
        match inspect_skill(skill, &mut claimed, TYCODE_PROJECTION) {
            Ok(entry) => inspected.push(entry),
            Err(reason) => refusals.push(SkillRefusal {
                name: skill.name.clone(),
                reason,
            }),
        }
    }

    let mut projected = Vec::new();
    if !inspected.is_empty() {
        let skills_dir = root.path.join(".tycode").join("skills");
        create_private_dir(&skills_dir)?;
        for entry in inspected {
            match write_wrapper(&skills_dir, &entry) {
                Ok(()) => projected.push(entry.projected),
                Err(reason) => {
                    discard_wrapper(&skills_dir, &entry.projected.name);
                    refusals.push(SkillRefusal {
                        name: entry.source_name,
                        reason,
                    });
                }
            }
        }
    }

    for refusal in &refusals {
        tracing::warn!("Tycode skill projection: {}", refusal.describe());
    }

    let steering = match tycode_skill_overlay(selection, &projected) {
        Some(overlay) => Some(match base_steering {
            Some(base) => format!("{base}\n\n{overlay}"),
            None => overlay,
        }),
        None => base_steering,
    };
    if let Some(steering) = steering {
        write_text_file(
            &root.path.join(".tycode").join("tyde_steering.md"),
            &steering,
        )?;
    }

    Ok(Some(TycodeCustomization {
        degraded_notice: (!refusals.is_empty()).then(|| tycode_degraded_notice(&refusals)),
        root,
    }))
}

/// Name the projected skills without restating a single body.
///
/// `AllInstalled` says nothing: Tycode's own system prompt already lists every
/// discovered skill with its description and mandates `invoke_skill`, so
/// re-listing them would rebuild the duplication native discovery removes.
/// `Explicit` enumerates, because a custom agent's selection is a deliberate
/// statement of intent — and either way a skill whose store name could not be
/// used verbatim is shown under both names, so the name the model must invoke is
/// never a mystery.
fn tycode_skill_overlay(selection: SkillSelection, projected: &[ProjectedSkill]) -> Option<String> {
    let renamed = projected
        .iter()
        .filter(|skill| skill.name != skill.source_name)
        .map(|skill| {
            format!(
                "- {} (installed in Tyde as '{}')",
                skill.name, skill.source_name
            )
        })
        .collect::<Vec<_>>();
    match selection {
        SkillSelection::AllInstalled => (!renamed.is_empty()).then(|| {
            format!(
                "Skills installed in Tyde are available through `invoke_skill`. These are \
                 listed under a different name than the one Tyde shows, and must be invoked by \
                 the first name:\n{}",
                renamed.join("\n")
            )
        }),
        SkillSelection::Explicit => {
            if projected.is_empty() {
                return None;
            }
            let mut lines = vec![
                "This agent selected these Tyde skills, available through `invoke_skill`:"
                    .to_string(),
            ];
            for skill in projected {
                let label = if skill.name == skill.source_name {
                    skill.name.clone()
                } else {
                    format!(
                        "{} (installed in Tyde as '{}')",
                        skill.name, skill.source_name
                    )
                };
                match skill.description.as_deref() {
                    Some(description) if !description.is_empty() => {
                        lines.push(format!("- {label} — {description}"));
                    }
                    _ => lines.push(format!("- {label}")),
                }
            }
            Some(lines.join("\n"))
        }
    }
}

/// User-visible notice for a Default session that started without some skills.
fn tycode_degraded_notice(refusals: &[SkillRefusal]) -> String {
    let mut lines = vec![format!(
        "Tyde started this Tycode session without {} of its selected skill(s):",
        refusals.len()
    )];
    for refusal in refusals {
        lines.push(format!("- {}", refusal.describe()));
    }
    lines.push(
        "The rest of the session's skills are available normally. Fix or remove the skills above \
         to expose them."
            .to_string(),
    );
    lines.join("\n")
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

fn apply_tycode_settings_overlay(
    current_settings: &Value,
    config: &BackendConfigValues,
    session_settings: &SessionSettingsValues,
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

    if mode == TycodeSettingsOverlayMode::SessionRuntime
        && let Some(SessionSettingValue::String(model)) =
            session_settings.0.get("tyde_conformance_model")
    {
        let pinned_model = json!({
            "model": model,
            "max_tokens": 8_000,
            "temperature": 1.0,
            "top_p": null,
            "reasoning_budget": "Off"
        });
        let mut agent_models = object
            .get("agent_models")
            .and_then(Value::as_object)
            .map(|configured| {
                configured
                    .keys()
                    .map(|agent| (agent.clone(), pinned_model.clone()))
                    .collect::<serde_json::Map<_, _>>()
            })
            .unwrap_or_default();
        for agent in [
            "auto_pr",
            "builder",
            "coder",
            "context",
            "coordinator",
            "debugger",
            "file_impl",
            "memory_manager",
            "memory_summarizer",
            "one_shot",
            "plan_judge",
            "planner",
            "review",
            "swarm",
            "tycode",
        ] {
            agent_models.insert(agent.to_owned(), pinned_model.clone());
        }
        if let Some(default_agent) = object.get("default_agent").and_then(Value::as_str) {
            agent_models.insert(default_agent.to_owned(), pinned_model);
        }
        object.insert("agent_models".to_owned(), Value::Object(agent_models));
        object.insert("swarm_models".to_owned(), json!([model]));
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
}

fn tycode_set_root_agent_supported() -> bool {
    true
}

fn tycode_root_agent_override_policy() -> TycodeRootAgentOverridePolicy {
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
                Ok(tycode_startup_internal_observation(value))
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
                    return self.complete_and_send_follow_up(stdin_tx);
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
        self.complete_and_send_follow_up(stdin_tx)
    }

    fn complete_and_send_follow_up(
        &mut self,
        stdin_tx: &mpsc::UnboundedSender<TycodeStdinCommand>,
    ) -> Result<TycodeStartupObservation, String> {
        self.phase = TycodeStartupPhase::Complete;
        self.send_follow_up(stdin_tx)?;
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
        if !matches!(&self.phase, TycodeStartupPhase::Complete) {
            return Err("Tycode follow-up cannot be sent before startup is complete".to_string());
        }
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
    ensure_tycode_home_dir(&home)?;
    cleanup_retired_projection_artifacts(&home);
    let profiles = tycode_config::discover_profiles_in(&home)?;

    let mut doc = TycodeNativeSettingsDoc {
        version: TYCODE_NATIVE_SETTINGS_VERSION,
        profiles: Vec::new(),
        tombstones: Vec::new(),
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
    if settings.get("actions").is_some() {
        return Err(
            "Tycode profile lifecycle actions must use InvokeSettingsAction, not a settings write"
                .to_owned(),
        );
    }
    let doc: TycodeNativeSettingsDoc = serde_json::from_value(settings)
        .map_err(|err| format!("invalid Tycode settings document: {err}"))?;
    if doc.version != TYCODE_NATIVE_SETTINGS_VERSION {
        return Err(format!(
            "unsupported Tycode settings document version {} (expected {})",
            doc.version, TYCODE_NATIVE_SETTINGS_VERSION
        ));
    }
    let home = tycode_home_dir()?;
    ensure_tycode_home_dir(&home)?;

    for profile_settings in &doc.profiles {
        let _profile_guard = tycode_profile_persist_lock(&profile_settings.name)
            .lock_owned()
            .await;
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

fn tycode_profile_persist_lock(name: &str) -> Arc<tokio::sync::Mutex<()>> {
    static LOCKS: std::sync::OnceLock<
        std::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    > = std::sync::OnceLock::new();
    let mut locks = LOCKS
        .get_or_init(|| std::sync::Mutex::new(HashMap::new()))
        .lock()
        .expect("Tycode profile lock registry poisoned");
    Arc::clone(
        locks
            .entry(name.to_owned())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
    )
}

#[derive(Deserialize)]
struct CreateProfileArguments {
    name: String,
    #[serde(default)]
    copy_from: Option<String>,
}

pub(crate) async fn invoke_settings_action(
    resource: &str,
    action: &str,
    arguments: Value,
) -> Result<Vec<String>, String> {
    let home = tycode_home_dir()?;
    ensure_tycode_home_dir(&home)?;
    match (resource, action) {
        ("profiles", "create") => {
            let arguments: CreateProfileArguments = serde_json::from_value(arguments)
                .map_err(|error| format!("invalid Tycode create-profile arguments: {error}"))?;
            let mut names = vec![arguments.name.clone()];
            if let Some(copy_from) = &arguments.copy_from {
                names.push(copy_from.clone());
            }
            names.sort();
            names.dedup();
            let mut guards = Vec::with_capacity(names.len());
            for name in names {
                guards.push(tycode_profile_persist_lock(&name).lock_owned().await);
            }
            tycode_config::create_profile_in(
                &home,
                &arguments.name,
                arguments.copy_from.as_deref(),
            )?;
            drop(guards);
            Ok(Vec::new())
        }
        (resource, "delete") if resource.starts_with("profiles/") => {
            let name = resource.trim_start_matches("profiles/");
            if name.is_empty() || name.contains('/') {
                return Err(format!("invalid Tycode profile resource {resource:?}"));
            }
            let _guard = tycode_profile_persist_lock(name).lock_owned().await;
            tycode_config::delete_profile_in(&home, name)?;
            Ok(vec![resource.to_owned()])
        }
        _ => Err(format!(
            "Tycode settings resource {resource:?} does not support action {action:?}"
        )),
    }
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
    let saved = run_tycode_settings_operation(
        command,
        purpose,
        TycodeSettingsOperation::Save(settings.clone()),
    )
    .await?
    .snapshot
    .settings
    .ok_or_else(|| {
        format!(
            "Tycode settings schema omitted post-save settings for profile '{}'",
            profile.name
        )
    })?;
    if saved != settings {
        return Err(format!(
            "Tycode profile '{}' did not retain the settings it accepted",
            profile.name
        ));
    }
    Ok(())
}

pub(crate) async fn persist_backend_config(values: BackendConfigValues) -> Result<(), String> {
    if values.0.is_empty() {
        return Ok(());
    }
    let home = tycode_home_dir()?;
    let profile = tycode_config::resolve_profile_ref_in(&home, None)?;
    let _profile_guard = tycode_profile_persist_lock(&profile.name)
        .lock_owned()
        .await;
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
        Some(
            "Settings" | "TimingUpdate" | "TypingStatusChanged" | "RootAgentChanged" | "TaskUpdate",
        ) => TycodeStartupObservation::Suppress,
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
    fn capabilities() -> tyde_agent_adapter::BackendCapabilities {
        [
            tyde_agent_adapter::BackendCapability::ListSessions,
            tyde_agent_adapter::BackendCapability::ResumeSession,
            tyde_agent_adapter::BackendCapability::Interrupt,
            tyde_agent_adapter::BackendCapability::StartupMcpServers,
            tyde_agent_adapter::BackendCapability::AgentControlTools,
            tyde_agent_adapter::BackendCapability::TurnUsageReported,
            tyde_agent_adapter::BackendCapability::OrchestrationEvents,
            tyde_agent_adapter::BackendCapability::RetryTelemetry,
            tyde_agent_adapter::BackendCapability::WorkspaceInstructions,
            tyde_agent_adapter::BackendCapability::Customization,
            tyde_agent_adapter::BackendCapability::GenericOtherTool,
        ]
        .into()
    }

    fn session_settings_schema() -> protocol::SessionSettingsSchema {
        tycode_session_settings_schema()
    }

    fn compaction_capability(&self) -> BackendCompactionCapability {
        BackendCompactionCapability::context_unavailable(
            BackendCompactionUnavailableReason::AdapterHasNoManualTransport,
        )
    }

    async fn begin_compaction(&self, _request: BackendCompactionRequest) -> BackendCompactionStart {
        BackendCompactionStart::NotDispatched {
            reason: BackendCompactionNotDispatchedReason::NativeUnavailable(
                BackendCompactionUnavailableReason::AdapterHasNoManualTransport,
            ),
            fallback_safe: true,
        }
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
            if let Some(customization) = materialized_customization.as_ref() {
                workspace_roots.push(customization.root.path.to_string_lossy().to_string());
                // A Default session that dropped a skill says so. The channel is
                // unbounded and its receiver was handed back before this task
                // ran, so the notice arrives even though nothing is listening
                // yet.
                if let Some(notice) = customization.degraded_notice.as_deref() {
                    let _ = events_tx.send(tycode_warning_chat_event(notice));
                }
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
                tycode_root_agent_override_policy(),
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
            for diagnostic in stream_state.take_orphaned_completion_diagnostics("transport closed")
            {
                let _ = events_tx.send(diagnostic);
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
            if let Some(customization) = materialized_customization.as_ref() {
                workspace_roots.push(customization.root.path.to_string_lossy().to_string());
                // A Default session that dropped a skill says so. The channel is
                // unbounded and its receiver was handed back before this task
                // ran, so the notice arrives even though nothing is listening
                // yet.
                if let Some(notice) = customization.degraded_notice.as_deref() {
                    let _ = events_tx.send(tycode_warning_chat_event(notice));
                }
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
                tycode_root_agent_override_policy(),
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

                if resume_replay_complete_tx.is_some() {
                    let observation = replay_barrier.observe(&value);
                    if observation.suppress_current_event {
                        tracing::debug!(
                            event = %tycode_event_diagnostic(&value),
                            "Suppressed Tycode native replay already owned by host bootstrap"
                        );
                        continue;
                    }
                    if observation.replay_complete {
                        if let Some(tx) = resume_replay_complete_tx.take() {
                            let _ = tx.send(());
                        }
                        continue;
                    }
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
            for diagnostic in stream_state.take_orphaned_completion_diagnostics("transport closed")
            {
                let _ = events_tx.send(diagnostic);
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
    conversation_cleared: bool,
    replay_events_remaining: usize,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct TycodeResumeReplayObservation {
    suppress_current_event: bool,
    replay_complete: bool,
}

impl TycodeResumeReplayBarrier {
    fn new(session_id: String, replay_events_remaining: usize) -> Self {
        Self {
            session_id,
            replay_started: false,
            conversation_cleared: false,
            replay_events_remaining,
        }
    }

    fn observe(&mut self, value: &Value) -> TycodeResumeReplayObservation {
        if !self.replay_started {
            if is_tycode_session_started(value, &self.session_id) {
                self.replay_started = true;
                self.replay_events_remaining = self.replay_events_remaining.saturating_sub(1);
                return TycodeResumeReplayObservation::default();
            }
            return TycodeResumeReplayObservation {
                suppress_current_event: is_tycode_task_update(value),
                replay_complete: false,
            };
        }

        if !self.conversation_cleared {
            if is_tycode_conversation_cleared(value) {
                self.conversation_cleared = true;
                self.replay_events_remaining = self.replay_events_remaining.saturating_sub(1);
            }
            return TycodeResumeReplayObservation::default();
        }

        if self.replay_events_remaining != 0 {
            self.replay_events_remaining -= 1;
            return TycodeResumeReplayObservation {
                suppress_current_event: true,
                replay_complete: false,
            };
        }

        TycodeResumeReplayObservation {
            suppress_current_event: false,
            replay_complete: is_tycode_sessions_list(value),
        }
    }
}

fn is_tycode_task_update(value: &Value) -> bool {
    value.get("kind").and_then(Value::as_str) == Some("TaskUpdate")
}

fn is_tycode_conversation_cleared(value: &Value) -> bool {
    value.get("kind").and_then(Value::as_str) == Some("ConversationCleared")
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
    if is_tycode_internal_completion_tool_event(value) {
        return Vec::new();
    }
    if matches!(
        value.get("kind").and_then(Value::as_str),
        Some("ToolRequest" | "ToolExecutionCompleted")
    ) {
        eprintln!("TYDE TYCODE GENERIC TOOL EVENT {value}");
    }
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
        Some("ToolRequest") => {
            if let Some(args) = normalized
                .get_mut("data")
                .and_then(|data| data.get_mut("tool_type"))
                .and_then(|tool_type| tool_type.get_mut("args"))
                && args.get("server").is_some()
                && args.get("tool").is_some()
                && let Some(arguments) = args.get("arguments").cloned()
            {
                *args = arguments;
            }
        }
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
        Some("ToolExecutionCompleted") => {
            if let Some(data) = normalized.get_mut("data")
                && data.get("success").and_then(Value::as_bool) == Some(false)
            {
                let message = data
                    .get("error")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .unwrap_or_else(|| "Tycode tool failed".to_owned());
                let cancelled = {
                    let lower = message.to_ascii_lowercase();
                    lower.contains("cancelled") || lower.contains("canceled")
                };
                if cancelled {
                    data["tool_result"] = serde_json::json!({
                        "kind": "Cancelled",
                        "message": message,
                    });
                } else if !matches!(
                    data.pointer("/tool_result/kind").and_then(Value::as_str),
                    Some("Error" | "Cancelled")
                ) {
                    let detailed_message = data
                        .get("tool_result")
                        .and_then(|result| serde_json::to_string_pretty(result).ok())
                        .unwrap_or_else(|| message.clone());
                    data["tool_result"] = serde_json::json!({
                        "kind": "Error",
                        "short_message": message,
                        "detailed_message": detailed_message,
                    });
                }
            }
        }
        _ => {}
    }
    normalized
}

fn normalize_tycode_chat_message(message: &mut Value) {
    if let Some(tool_calls) = message.get_mut("tool_calls").and_then(Value::as_array_mut) {
        tool_calls.retain(|tool_call| {
            tool_call.get("name").and_then(Value::as_str) != Some("complete_task")
        });
    }
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

fn is_tycode_internal_completion_tool_event(value: &Value) -> bool {
    matches!(
        value.get("kind").and_then(Value::as_str),
        Some("ToolRequest" | "ToolExecutionCompleted")
    ) && value
        .get("data")
        .and_then(|data| data.get("tool_name"))
        .and_then(Value::as_str)
        == Some("complete_task")
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
            | "ToolProgress"
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

/// A non-fatal notice: the session is running, but not with everything the user
/// configured. `Warning` rather than `Error` so it does not read as a failed
/// start.
fn tycode_warning_chat_event(message: impl Into<String>) -> ChatEvent {
    ChatEvent::MessageAdded(ChatMessage {
        message_id: None,
        timestamp: unix_now_ms(),
        sender: MessageSender::Warning,
        content: message.into(),
        reasoning: None,
        tool_calls: Vec::new(),
        model_info: None,
        token_usage: None,
        context_breakdown: None,
        images: None,
    })
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
    typing_active: bool,
    pending_typing_start: bool,
    message_id: Option<String>,
    agent: Option<String>,
    model: Option<String>,
    accumulated_text: String,
    accumulated_reasoning: String,
    synthetic_completion: Option<SyntheticTycodeCompletion>,
    normalization_failures: HashMap<String, PendingToolNormalizationFailure>,
    emitted_tool_request_ids: HashSet<String>,
    pending_tool_completions: VecDeque<ToolExecutionCompletedData>,
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
        for event in events {
            if matches!(
                &event,
                ChatEvent::ToolRequest(request) if request.tool_name == "complete_task"
            ) || matches!(
                &event,
                ChatEvent::ToolExecutionCompleted(completion)
                    if completion.tool_name == "complete_task"
            ) {
                continue;
            }
            let completion_needs_request = matches!(
                &event,
                ChatEvent::ToolExecutionCompleted(completion)
                    if !self.emitted_tool_request_ids.contains(&completion.tool_call_id)
            );
            if completion_needs_request {
                let ChatEvent::ToolExecutionCompleted(completion) = event else {
                    unreachable!("completion predicate matched a non-completion event")
                };
                self.pending_tool_completions.push_back(completion);
                continue;
            }
            let (mut event, _) = normalize_tyde_chat_event(event, &mut self.normalization_failures);
            if let ChatEvent::ToolRequest(request) = &event
                && !self
                    .emitted_tool_request_ids
                    .insert(request.tool_call_id.clone())
            {
                eprintln!(
                    "TYDE TYCODE BATCH TOOL suppressed_duplicate_request={}",
                    request.tool_call_id
                );
                continue;
            }
            if matches!(&event, ChatEvent::TypingStatusChanged(true)) {
                if self.typing_active || self.pending_typing_start {
                    tracing::warn!("suppressed duplicate Tycode TypingStatusChanged(true)");
                } else {
                    self.pending_typing_start = true;
                }
                continue;
            }

            let user_echo = matches!(
                &event,
                ChatEvent::MessageAdded(ChatMessage {
                    sender: MessageSender::User,
                    ..
                })
            );
            if user_echo {
                self.inject_stream_identity(&mut event);
                self.update(&event);
                output.push(event);
                self.flush_pending_typing_start(&mut output);
                continue;
            }

            if matches!(&event, ChatEvent::StreamStart(_)) {
                if !self.typing_active && !self.pending_typing_start {
                    tracing::warn!(
                        "Tycode StreamStart arrived without TypingStatusChanged(true); synthesized start"
                    );
                    self.pending_typing_start = true;
                }
                self.flush_pending_typing_start(&mut output);
            } else if matches!(&event, ChatEvent::TypingStatusChanged(false)) {
                self.flush_pending_typing_start(&mut output);
                if !self.typing_active {
                    tracing::warn!("suppressed idle Tycode TypingStatusChanged(false)");
                    continue;
                }
            }

            self.synthesize_tool_requests_from_stream_end(&event, &mut output);
            if let Some(mut events) = self.late_authoritative_stream_end_events(&event) {
                events.extend(self.take_orphaned_completion_diagnostics("turn ended"));
                output.extend(events);
                continue;
            }
            if let Some(stream_end) = self.synthesize_stream_end_before(&event) {
                output.push(stream_end);
            }
            self.inject_stream_identity(&mut event);
            self.update(&event);
            if matches!(&event, ChatEvent::TypingStatusChanged(false)) {
                self.typing_active = false;
            }
            let emitted_request_id = match &event {
                ChatEvent::ToolRequest(request) => Some(request.tool_call_id.clone()),
                _ => None,
            };
            let terminal = match &event {
                ChatEvent::StreamEnd(_) => Some("turn ended"),
                ChatEvent::OperationCancelled(_) => Some("turn cancelled"),
                ChatEvent::TypingStatusChanged(false) => Some("backend became idle"),
                _ => None,
            };
            output.push(event);
            if let Some(tool_call_id) = emitted_request_id {
                self.flush_completions_after_request(&tool_call_id, &mut output);
            }
            if let Some(terminal) = terminal {
                output.extend(self.take_orphaned_completion_diagnostics(terminal));
            }
        }

        output
    }

    fn synthesize_tool_requests_from_stream_end(
        &mut self,
        event: &ChatEvent,
        output: &mut Vec<ChatEvent>,
    ) {
        let ChatEvent::StreamEnd(end) = event else {
            return;
        };
        for tool_call in &end.message.tool_calls {
            if tool_call.name == "complete_task"
                || !self.emitted_tool_request_ids.insert(tool_call.id.clone())
            {
                continue;
            }
            eprintln!(
                "TYDE TYCODE BATCH TOOL announced_request={} tool={}",
                tool_call.id, tool_call.name
            );
            let request = ChatEvent::ToolRequest(ToolRequest {
                tool_call_id: tool_call.id.clone(),
                tool_name: tool_call.name.clone(),
                tool_type: ToolRequestType::Other {
                    args: tool_call.arguments.clone(),
                },
            });
            let (request, _) = normalize_tyde_chat_event(request, &mut self.normalization_failures);
            output.push(request);
            self.flush_completions_after_request(&tool_call.id, output);
        }
    }

    fn flush_completions_after_request(&mut self, tool_call_id: &str, output: &mut Vec<ChatEvent>) {
        let mut remaining = VecDeque::new();
        while let Some(completion) = self.pending_tool_completions.pop_front() {
            if completion.tool_call_id == tool_call_id {
                let (completion, _) = normalize_tyde_chat_event(
                    ChatEvent::ToolExecutionCompleted(completion),
                    &mut self.normalization_failures,
                );
                output.push(completion);
            } else {
                remaining.push_back(completion);
            }
        }
        self.pending_tool_completions = remaining;
    }

    fn take_orphaned_completion_diagnostics(&mut self, terminal: &str) -> Vec<ChatEvent> {
        std::mem::take(&mut self.pending_tool_completions)
            .into_iter()
            .map(|completion| {
                let message = format!(
                    "Tycode emitted completion for tool {:?} with call id {:?}, but no authoritative ToolRequest arrived before {terminal}",
                    completion.tool_name, completion.tool_call_id
                );
                tracing::error!(
                    tool_name = %completion.tool_name,
                    tool_call_id = %completion.tool_call_id,
                    terminal,
                    "Tycode tool completion had no authoritative request"
                );
                tycode_error_chat_event(message)
            })
            .collect()
    }

    fn flush_pending_typing_start(&mut self, output: &mut Vec<ChatEvent>) {
        if !self.pending_typing_start {
            return;
        }
        self.pending_typing_start = false;
        self.typing_active = true;
        let event = ChatEvent::TypingStatusChanged(true);
        self.update(&event);
        output.push(event);
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
