pub mod acp;
pub mod agent_control_progress;
pub mod antigravity;
pub mod claude;
pub mod codex;
pub mod hermes;
pub mod kiro;
pub mod mock;
pub mod setup;
pub mod subprocess;
pub mod turn_emitter;
pub mod tycode;

use std::collections::HashMap;

use protocol::{
    AgentErrorCode, AgentInput, BackendAccessMode, BackendConfigFieldType, BackendConfigSchema,
    BackendConfigSnapshot, BackendConfigSnapshotStatus, BackendConfigValues, BackendKind,
    BackendTierConfig, ChatEvent, CustomAgentId, ImageData, SendMessagePayload, SessionId,
    SessionSettingFieldType, SessionSettingValue, SessionSettingsSchema, SessionSettingsValues,
    SpawnCostHint,
};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use self::subprocess::ImageAttachment;
use crate::agent::customization::ResolvedSpawnConfig;

pub(crate) const READ_ONLY_ACCESS_MODE_INSTRUCTIONS: &str = concat!(
    "Backend access mode is read-only (best effort). Treat the workspace as ",
    "read-only: do not create, edit, or delete files, and do not run commands ",
    "that modify files, processes, or external state. You MAY freely inspect ",
    "anything — read files, list directories, and run read-only shell commands ",
    "such as `git status`/`log`/`diff`, `grep`/`rg`, `cat`, `ls`, and ",
    "`find` — to investigate the code. Prefer read/inspection tools; do not ",
    "use write/edit/apply-patch tools."
);

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
    pub transport: StartupMcpTransport,
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
    pub cost_hint: Option<SpawnCostHint>,
    pub custom_agent_id: Option<CustomAgentId>,
    pub startup_mcp_servers: Vec<StartupMcpServer>,
    pub session_settings: Option<SessionSettingsValues>,
    /// Host-level deep configuration for this backend (see
    /// [`protocol::BackendConfigSchema`]). Empty when unconfigured.
    pub backend_config: BackendConfigValues,
    pub resolved_spawn_config: ResolvedSpawnConfig,
}

/// Output stream of ChatEvents from a backend session.
/// The agent actor reads from this while independently sending AgentInput
/// through the Backend handle — true duplex.
pub struct EventStream {
    rx: mpsc::UnboundedReceiver<ChatEvent>,
    resume_replay_complete: Option<oneshot::Receiver<()>>,
}

impl EventStream {
    pub fn new(rx: mpsc::UnboundedReceiver<ChatEvent>) -> Self {
        Self {
            rx,
            resume_replay_complete: None,
        }
    }

    pub fn new_with_resume_replay_barrier(
        rx: mpsc::UnboundedReceiver<ChatEvent>,
        resume_replay_complete: oneshot::Receiver<()>,
    ) -> Self {
        Self {
            rx,
            resume_replay_complete: Some(resume_replay_complete),
        }
    }

    /// Receive the next ChatEvent from the backend.
    /// Returns None when the backend has terminated.
    pub async fn recv(&mut self) -> Option<ChatEvent> {
        self.rx.recv().await
    }

    /// Non-blocking receive used to drain already-buffered backend events
    /// (e.g. queued resume-replay events) without awaiting new ones.
    pub fn try_recv(&mut self) -> Result<ChatEvent, mpsc::error::TryRecvError> {
        self.rx.try_recv()
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

/// A coding agent backend session handle.
///
/// Created via `Backend::spawn()` which returns `(Self, EventStream)`.
/// The handle is used to send input; the EventStream is used to read output.
/// Backends are not object-safe — the agent actor knows the concrete type.
pub trait Backend: Send + 'static {
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
        BackendKind::Kiro => kiro::KiroBackend::session_settings_schema(),
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
        BackendKind::Hermes => hermes::HermesBackend::backend_config_schema(),
        BackendKind::Tycode => tycode::TycodeBackend::backend_config_schema(),
        BackendKind::Kiro | BackendKind::Claude | BackendKind::Codex | BackendKind::Antigravity => {
            None
        }
    }
}

pub(crate) async fn backend_config_snapshot_for_backend(
    backend_kind: BackendKind,
    workspace_roots: &[String],
) -> Option<BackendConfigSnapshot> {
    let result = match backend_kind {
        BackendKind::Hermes => hermes::probe_backend_config_snapshot(workspace_roots).await,
        BackendKind::Tycode => tycode::backend_config_snapshot().await,
        BackendKind::Kiro | BackendKind::Claude | BackendKind::Codex | BackendKind::Antigravity => {
            return None;
        }
    };

    Some(match result {
        Ok(values) => BackendConfigSnapshot {
            backend_kind,
            status: BackendConfigSnapshotStatus::Ready,
            values,
            message: None,
        },
        Err(error) => BackendConfigSnapshot {
            backend_kind,
            status: BackendConfigSnapshotStatus::Unavailable,
            values: BackendConfigValues::default(),
            message: Some(error),
        },
    })
}

/// Drop keys/values that the backend's config schema does not accept. A backend
/// with no config schema sanitizes to empty (it stores no deep config).
pub(crate) fn sanitize_backend_config_values(
    backend_kind: BackendKind,
    values: &BackendConfigValues,
) -> BackendConfigValues {
    let Some(schema) = backend_config_schema_for_backend(backend_kind) else {
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
    if values.0.is_empty() {
        return Ok(BackendConfigValues::default());
    }
    let Some(schema) = backend_config_schema_for_backend(backend_kind) else {
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

pub(crate) fn merge_backend_config_update(
    backend_kind: BackendKind,
    previous: Option<&BackendConfigValues>,
    incoming: &BackendConfigValues,
) -> Result<BackendConfigValues, String> {
    if incoming.0.is_empty() {
        return Ok(BackendConfigValues::default());
    }

    let update = validate_backend_config_values(backend_kind, incoming)?;
    let mut merged = previous
        .map(|values| sanitize_backend_config_values(backend_kind, values))
        .unwrap_or_default();
    for (key, value) in update.0 {
        if matches!(value, SessionSettingValue::Null) {
            merged.0.remove(&key);
        } else {
            merged.0.insert(key, value);
        }
    }
    Ok(merged)
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

/// The effective config value for a text/secret field, trimmed and non-empty.
pub(crate) fn backend_config_text<'a>(
    values: &'a BackendConfigValues,
    key: &str,
) -> Option<&'a str> {
    match values.0.get(key) {
        Some(SessionSettingValue::String(value)) if !value.trim().is_empty() => Some(value.trim()),
        _ => None,
    }
}

pub(crate) fn resolve_backend_session_settings(
    backend_kind: BackendKind,
    config: &BackendSpawnConfig,
) -> SessionSettingsValues {
    match backend_kind {
        BackendKind::Tycode => tycode::resolve_session_settings(config),
        BackendKind::Kiro => kiro::resolve_session_settings(config),
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
        .map(|field| (field.key.as_str(), &field.field_type))
        .collect::<HashMap<_, _>>();

    for (key, value) in &values.0 {
        let Some(field_type) = fields_by_key.get(key.as_str()) else {
            return Err(format!(
                "unknown session setting '{}' for backend {:?}",
                key, schema.backend_kind
            ));
        };
        validate_session_setting_value(key, value, field_type)?;
    }

    Ok(())
}

pub(crate) fn validate_runtime_session_settings_update(
    backend_kind: BackendKind,
    values: &SessionSettingsValues,
) -> Result<(), String> {
    match backend_kind {
        BackendKind::Tycode => tycode::validate_runtime_session_settings_update(values),
        BackendKind::Kiro
        | BackendKind::Claude
        | BackendKind::Codex
        | BackendKind::Antigravity
        | BackendKind::Hermes => Ok(()),
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
        if validate_session_setting_value(key, value, &field.field_type).is_ok() {
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

/// The built-in Low/High tier mapping for a backend, used to seed the
/// user-editable per-backend tier config when complexity tiers are first
/// enabled (and as the spawn-time fallback for backends with no config).
pub(crate) fn builtin_tier_config(kind: BackendKind) -> BackendTierConfig {
    let defaults: fn(SpawnCostHint) -> SessionSettingsValues = match kind {
        BackendKind::Claude => claude::claude_cost_hint_defaults,
        BackendKind::Codex => codex::codex_cost_hint_defaults,
        BackendKind::Antigravity => antigravity::antigravity_cost_hint_defaults,
        BackendKind::Kiro => kiro::kiro_cost_hint_defaults,
        BackendKind::Tycode | BackendKind::Hermes => |_| SessionSettingsValues::default(),
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
    if !config.skills.is_empty() {
        let skill_blocks = config
            .skills
            .iter()
            .map(|skill| format!("Skill: {}\n{}", skill.name, skill.body.trim()))
            .collect::<Vec<_>>()
            .join("\n\n");
        sections.push(format!("Skills:\n{skill_blocks}"));
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
    field_type: &SessionSettingFieldType,
) -> Result<(), String> {
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
        (SessionSettingValue::String(actual), SessionSettingFieldType::Select { options, .. }) => {
            if options.iter().any(|option| option.value == *actual) {
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

#[cfg(test)]
mod tests {
    use protocol::{BackendAccessMode, BackendConfigValues, BackendKind, SessionSettingValue};

    use super::{
        READ_ONLY_ACCESS_MODE_INSTRUCTIONS, merge_backend_config_update,
        render_combined_spawn_instructions, sanitize_backend_config_values,
        validate_backend_config_values,
    };
    use crate::agent::customization::ResolvedSpawnConfig;

    #[test]
    fn backend_config_sanitization_drops_unknown_and_mistyped_values() {
        let mut good = BackendConfigValues::default();
        good.0.insert(
            "default_model".to_string(),
            SessionSettingValue::String("anthropic/claude-sonnet-5".to_string()),
        );

        // Unknown keys and wrong-typed values are silently dropped; valid
        // Text values are kept.
        let mut mixed = BackendConfigValues::default();
        mixed.0.insert(
            "default_model".to_string(),
            SessionSettingValue::String("x/y".to_string()),
        );
        mixed
            .0
            .insert("bogus_key".to_string(), SessionSettingValue::Bool(true));
        mixed.0.insert(
            "default_provider".to_string(),
            SessionSettingValue::Integer(3),
        );

        let sanitized = sanitize_backend_config_values(BackendKind::Hermes, &mixed);
        assert_eq!(sanitized.0.len(), 1);
        assert!(sanitized.0.contains_key("default_model"));

        // A backend with no config schema sanitizes everything away.
        let sanitized = sanitize_backend_config_values(BackendKind::Claude, &good);
        assert!(sanitized.0.is_empty());
    }

    #[test]
    fn backend_config_update_validation_surfaces_bad_keys_and_merges() {
        let mut previous = BackendConfigValues::default();
        previous.0.insert(
            "default_model".to_string(),
            SessionSettingValue::String("anthropic/claude-sonnet-5".to_string()),
        );

        let mut update = BackendConfigValues::default();
        update.0.insert(
            "default_provider".to_string(),
            SessionSettingValue::String("anthropic".to_string()),
        );
        let merged = merge_backend_config_update(BackendKind::Hermes, Some(&previous), &update)
            .expect("valid update merges");
        assert_eq!(merged.0.len(), 2);
        assert!(merged.0.contains_key("default_model"));
        assert!(merged.0.contains_key("default_provider"));

        let mut clear = BackendConfigValues::default();
        clear
            .0
            .insert("default_model".to_string(), SessionSettingValue::Null);
        let merged = merge_backend_config_update(BackendKind::Hermes, Some(&merged), &clear)
            .expect("null update clears");
        assert!(!merged.0.contains_key("default_model"));
        assert!(merged.0.contains_key("default_provider"));

        let mut unknown = BackendConfigValues::default();
        unknown
            .0
            .insert("bogus_key".to_string(), SessionSettingValue::Bool(true));
        let err = validate_backend_config_values(BackendKind::Hermes, &unknown)
            .expect_err("unknown backend config key should be rejected");
        assert!(
            err.contains("bogus_key"),
            "error should include rejected key: {err}"
        );
    }

    #[test]
    fn read_only_spawn_instructions_allow_inspection_and_forbid_mutation() {
        let instructions = render_combined_spawn_instructions(&ResolvedSpawnConfig {
            access_mode: BackendAccessMode::ReadOnly,
            ..ResolvedSpawnConfig::default()
        })
        .expect("read-only instructions");

        assert_eq!(instructions, READ_ONLY_ACCESS_MODE_INSTRUCTIONS);
        assert!(instructions.contains("You MAY freely inspect anything"));
        assert!(instructions.contains("read files"));
        assert!(instructions.contains("list directories"));
        assert!(instructions.contains("run read-only shell commands"));
        assert!(instructions.contains("`git status`/`log`/`diff`"));
        assert!(instructions.contains("`grep`/`rg`"));
        assert!(instructions.contains("`cat`"));
        assert!(instructions.contains("`ls`"));
        assert!(instructions.contains("`find`"));
        assert!(instructions.contains("do not create, edit, or delete files"));
        assert!(
            instructions
                .contains("do not run commands that modify files, processes, or external state")
        );
        assert!(instructions.contains("do not use write/edit/apply-patch tools"));
        assert!(!instructions.contains("do not run shell commands"));
    }
}
