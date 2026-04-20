pub mod acp;
pub mod claude;
pub mod codex;
pub mod gemini;
pub mod kiro;
pub mod mock;
pub mod setup;
pub mod subprocess;
pub mod tycode;

use std::collections::HashMap;

use protocol::{
    AgentInput, BackendKind, ChatEvent, CustomAgentId, ImageData, SendMessagePayload, SessionId,
    SessionSettingFieldType, SessionSettingValue, SessionSettingsSchema, SessionSettingsValues,
    SpawnCostHint,
};
use serde_json::Value;
use tokio::sync::mpsc;

use self::subprocess::ImageAttachment;
use crate::agent::customization::ResolvedSpawnConfig;

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
    pub resolved_spawn_config: ResolvedSpawnConfig,
}

/// Output stream of ChatEvents from a backend session.
/// The agent actor reads from this while independently sending AgentInput
/// through the Backend handle — true duplex.
pub struct EventStream {
    rx: mpsc::Receiver<ChatEvent>,
}

impl EventStream {
    pub fn new(rx: mpsc::Receiver<ChatEvent>) -> Self {
        Self { rx }
    }

    /// Receive the next ChatEvent from the backend.
    /// Returns None when the backend has terminated.
    pub async fn recv(&mut self) -> Option<ChatEvent> {
        self.rx.recv().await
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

    /// Create a new backend session.
    /// Returns a handle to send input and an EventStream to read output.
    /// The backend must start the session with `initial_input` and know its
    /// native resumable session ID before returning.
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

    /// Interrupt the currently active turn, if any.
    /// Returns false if the backend has terminated or doesn't support interruption.
    fn interrupt(&self) -> impl std::future::Future<Output = bool> + Send;

    /// Shut down the live backend session and release any subprocess resources.
    fn shutdown(self) -> impl std::future::Future<Output = ()> + Send
    where
        Self: Sized;
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
        BackendKind::Gemini => gemini::GeminiBackend::session_settings_schema(),
    }
}

pub(crate) fn resolve_backend_session_settings(
    backend_kind: BackendKind,
    config: &BackendSpawnConfig,
) -> SessionSettingsValues {
    match backend_kind {
        BackendKind::Tycode => SessionSettingsValues::default(),
        BackendKind::Kiro => kiro::resolve_session_settings(config),
        BackendKind::Claude => claude::resolve_session_settings(config),
        BackendKind::Codex => codex::resolve_session_settings(config),
        BackendKind::Gemini => gemini::resolve_session_settings(config),
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
