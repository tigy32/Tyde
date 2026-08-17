//! Typed model of Tyde's host-level settings.
//!
//! This crate is the home of the typed host-settings family (`HostSettings`
//! and its sub-structs), separate from the wire protocol. It depends on
//! `protocol` for shared domain IDs and newtypes (`BackendKind`,
//! `SessionSettingsValues`, `BrokerUrl`, `CodeIntelProviderId`,
//! `AcpAgentSpec`, ...); `protocol` never depends on this crate.
//!
//! This crate is wasm-clean: no tokio or native-only dependencies, and it
//! disables `protocol`'s default `framing` feature.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::sync::OnceLock;

use protocol::{
    AcpAgentSpec, BackendConfigValues, BackendKind, BrokerUrl, CodeIntelProviderId,
    LaunchProfileId, SessionSettingsValues, SpawnCostHint, VoiceAvailability,
};
use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct HostLaunchProfileConfig {
    pub id: LaunchProfileId,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub backend_kind: BackendKind,
    #[serde(default)]
    pub session_settings: SessionSettingsValues,
    /// Required when `backend_kind` is [`BackendKind::Acp`], rejected
    /// otherwise. Validated in the settings store rather than the type system
    /// so an invalid persisted profile surfaces a named error instead of
    /// failing to deserialize the whole settings file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acp: Option<AcpAgentSpec>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct HostSettings {
    #[serde(default)]
    pub enabled_backends: Vec<BackendKind>,
    #[serde(default)]
    pub default_backend: Option<BackendKind>,
    #[serde(default)]
    pub enable_mobile_connections: bool,
    #[serde(default)]
    pub mobile_broker_url: Option<BrokerUrl>,
    #[serde(default)]
    pub mobile_broker_auth: MobileBrokerAuthSettings,
    #[serde(default)]
    pub tyde_debug_mcp_enabled: bool,
    #[serde(default = "default_agent_control_mcp_enabled")]
    pub tyde_agent_control_mcp_enabled: bool,
    #[serde(default = "default_agent_control_max_depth")]
    pub tyde_agent_control_max_depth: u8,
    /// When false (default), spawn cost hints are ignored: every spawn uses
    /// the backend's own default model/effort and the hint is hidden from
    /// spawn UIs and the agent-control MCP tool schema.
    #[serde(default)]
    pub complexity_tiers_enabled: bool,
    /// Per-backend overrides for what the Low/High complexity tiers mean.
    /// Backends without an entry fall back to built-in defaults.
    #[serde(default)]
    pub backend_tier_configs: HashMap<BackendKind, BackendTierConfig>,
    #[serde(default = "default_background_agent_features")]
    pub background_agent_features: BackgroundAgentFeaturesSettings,
    #[serde(default)]
    pub supervisor: SupervisorSettings,
    #[serde(default)]
    pub code_intel: CodeIntelSettings,
    /// Per-backend deep configuration (e.g. Hermes default model/provider).
    /// Host-level and persistent, distinct from lightweight per-session
    /// settings. Keys/values are described by each backend's
    /// [`protocol::BackendConfigSchema`]. Backends without an entry use their defaults.
    #[serde(default)]
    pub backend_config: HashMap<BackendKind, BackendConfigValues>,
    /// Explicit server-owned Launch Profiles. These are host-level presets
    /// over backend session settings; they are never inferred from model names.
    #[serde(default)]
    pub launch_profiles: BTreeMap<LaunchProfileId, HostLaunchProfileConfig>,
    /// Provider slugs Tyde must not offer for a given Hermes profile, keyed by
    /// profile name. Tyde-owned because Hermes itself has no provider
    /// enable/disable flag: it reports every provider it can find credentials
    /// for, and auto-harvested ones (GitHub Copilot via the `gh` CLI login)
    /// come back after a disconnect. Entries here are filtered out of Tyde's
    /// Hermes model options; they do not touch Hermes's own configuration, so
    /// a `hermes` session started outside Tyde still sees the provider.
    #[serde(default)]
    pub hermes_disabled_providers: HashMap<String, Vec<String>>,
    #[serde(default)]
    pub voice: VoiceSettings,
}

impl Default for HostSettings {
    fn default() -> Self {
        Self {
            enabled_backends: Vec::new(),
            default_backend: None,
            enable_mobile_connections: false,
            mobile_broker_url: None,
            mobile_broker_auth: MobileBrokerAuthSettings::default(),
            tyde_debug_mcp_enabled: false,
            tyde_agent_control_mcp_enabled: true,
            tyde_agent_control_max_depth: default_agent_control_max_depth(),
            complexity_tiers_enabled: false,
            backend_tier_configs: HashMap::new(),
            background_agent_features: default_background_agent_features(),
            supervisor: SupervisorSettings::default(),
            code_intel: CodeIntelSettings::default(),
            backend_config: HashMap::new(),
            launch_profiles: BTreeMap::new(),
            hermes_disabled_providers: HashMap::new(),
            voice: VoiceSettings::default(),
        }
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SecretString(String);

impl SecretString {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretString(<redacted>)")
    }
}

impl JsonSchema for SecretString {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> std::borrow::Cow<'static, str> {
        "SecretString".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "format": "password",
            "writeOnly": true
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MobileBrokerAuthSettings {
    #[serde(default = "default_mobile_broker_username")]
    pub username: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<SecretString>,
}

impl Default for MobileBrokerAuthSettings {
    fn default() -> Self {
        Self {
            username: default_mobile_broker_username(),
            password: None,
        }
    }
}

fn default_mobile_broker_username() -> String {
    "tyde".to_owned()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct VoiceSettings {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aws_profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aws_region: Option<String>,
    #[serde(default = "default_voice_nova_model")]
    pub nova_model: String,
    #[serde(default)]
    pub endpointing_sensitivity: VoiceEndpointingSensitivity,
    #[serde(default)]
    pub availability: VoiceAvailability,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum VoiceEndpointingSensitivity {
    High,
    Medium,
    #[default]
    Low,
}

impl VoiceEndpointingSensitivity {
    pub fn nova_value(self) -> &'static str {
        match self {
            Self::High => "HIGH",
            Self::Medium => "MEDIUM",
            Self::Low => "LOW",
        }
    }
}

fn default_voice_nova_model() -> String {
    "amazon.nova-2-sonic-v1:0".into()
}

impl Default for VoiceSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            aws_profile: None,
            aws_region: None,
            nova_model: default_voice_nova_model(),
            endpointing_sensitivity: VoiceEndpointingSensitivity::default(),
            availability: VoiceAvailability::default(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CodeIntelSettings {
    #[serde(default)]
    pub language_server_paths: HashMap<CodeIntelProviderId, HostExecutablePath>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct HostExecutablePath(pub String);

impl fmt::Display for HostExecutablePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BackgroundAgentFeaturesSettings {
    #[serde(default = "default_auto_generate_agent_names_enabled")]
    pub auto_generate_agent_names: bool,
    #[serde(default)]
    pub agent_activity_summaries: bool,
}

/// Agent supervisor: when an agent goes idle, a hidden one-shot model call
/// reviews the last user request, the task list, and the agent's final
/// message, then either accepts the turn as finished or sends a follow-up
/// message to kick the agent back to work. Costs money per idle transition,
/// so everything defaults off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SupervisorSettings {
    #[serde(default)]
    pub enabled: bool,
    /// Judge an agent whose session was restored from history as soon as its
    /// replayed transcript settles into idle. Off by default: reopening a saved
    /// session is not new work, so the supervisor waits for that agent's first
    /// live turn instead of spending a verdict — and possibly a kick — on a
    /// conversation the user only wanted to look at.
    #[serde(default)]
    pub supervise_restored_agents: bool,
    /// Interrupt a turn that has gone [`Self::stall_timeout_seconds`] without
    /// observable progress and let the supervisor decide how to make progress.
    /// Off by default; cancelling a running turn is destructive.
    #[serde(default)]
    pub stall_timeout_enabled: bool,
    /// Whole seconds without any observable turn progress before a stalled turn
    /// is interrupted. Progress is any backend event on the turn, so a slow but
    /// working agent never trips this. Only read when
    /// [`Self::stall_timeout_enabled`] is set.
    #[serde(default = "default_supervisor_stall_timeout_seconds")]
    pub stall_timeout_seconds: u32,
    /// When the supervisor judges the task complete, automatically compact
    /// (rotate-and-summarize) the agent so reusing it later starts from a
    /// small warm context instead of resuming a huge cold session.
    #[serde(default)]
    pub auto_compact_on_success: bool,
    /// Whole seconds of uninterrupted inactivity required before a successful
    /// supervision verdict may start automatic compaction.
    #[serde(default = "default_supervisor_auto_compact_inactivity_delay_seconds")]
    pub auto_compact_inactivity_delay_seconds: u32,
    /// Minimum latest-assistant context size required before a successful
    /// supervision verdict may trigger automatic compaction.
    #[serde(default = "default_supervisor_auto_compact_min_context_tokens")]
    pub auto_compact_min_context_tokens: u64,
    /// Maximum consecutive supervisor kicks without an intervening real user
    /// message. Prevents a supervisor/agent ping-pong loop.
    #[serde(default = "default_supervisor_max_kicks_per_task")]
    pub max_kicks_per_task: u8,
    /// Extra delayed paid attempts when a supervision call errors or returns
    /// output that does not parse to a verdict. 1 means two total calls.
    #[serde(default = "default_supervisor_retry_attempts")]
    pub retry_attempts: u8,
    /// Which model tier the supervision verdict runs on. `Low` is the cheap
    /// tier (like agent naming); `Default` uses the backend's own default
    /// model; `High` is the most capable configuration.
    #[serde(default)]
    pub cost_tier: SupervisorCostTier,
}

/// Model tier for supervision verdict calls, mapped to a [`SpawnCostHint`]
/// at spawn time (`Default` maps to no hint).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SupervisorCostTier {
    #[default]
    Low,
    Default,
    High,
}

impl SupervisorCostTier {
    pub fn as_cost_hint(self) -> Option<SpawnCostHint> {
        match self {
            Self::Low => Some(SpawnCostHint::Low),
            Self::Default => None,
            Self::High => Some(SpawnCostHint::High),
        }
    }
}

pub fn default_supervisor_max_kicks_per_task() -> u8 {
    3
}

pub fn default_supervisor_retry_attempts() -> u8 {
    1
}

pub const SUPERVISOR_RETRY_ATTEMPTS_MIN: u8 = 0;
pub const SUPERVISOR_RETRY_ATTEMPTS_MAX: u8 = 5;

pub fn default_supervisor_auto_compact_min_context_tokens() -> u64 {
    200_000
}

pub const SUPERVISOR_AUTO_COMPACT_INACTIVITY_DELAY_SECONDS_MIN: u32 = 1;
pub const SUPERVISOR_AUTO_COMPACT_INACTIVITY_DELAY_SECONDS_MAX: u32 = 86_400;

pub fn default_supervisor_auto_compact_inactivity_delay_seconds() -> u32 {
    300
}

pub const SUPERVISOR_STALL_TIMEOUT_SECONDS_MIN: u32 = 1;
pub const SUPERVISOR_STALL_TIMEOUT_SECONDS_MAX: u32 = 86_400;

pub fn default_supervisor_stall_timeout_seconds() -> u32 {
    1_800
}

impl Default for SupervisorSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            supervise_restored_agents: false,
            stall_timeout_enabled: false,
            stall_timeout_seconds: default_supervisor_stall_timeout_seconds(),
            auto_compact_on_success: false,
            auto_compact_inactivity_delay_seconds:
                default_supervisor_auto_compact_inactivity_delay_seconds(),
            auto_compact_min_context_tokens: default_supervisor_auto_compact_min_context_tokens(),
            max_kicks_per_task: default_supervisor_max_kicks_per_task(),
            retry_attempts: default_supervisor_retry_attempts(),
            cost_tier: SupervisorCostTier::default(),
        }
    }
}

/// Per-backend mapping from spawn complexity tiers to session-settings
/// overrides (e.g. `model`, `effort`). An empty map means "no override" —
/// the spawn runs on the backend's own defaults.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BackendTierConfig {
    #[serde(default)]
    pub low: SessionSettingsValues,
    #[serde(default)]
    pub high: SessionSettingsValues,
}

fn default_agent_control_mcp_enabled() -> bool {
    true
}

pub const TYDE_AGENT_CONTROL_MAX_DEPTH_MIN: u8 = 1;
pub const TYDE_AGENT_CONTROL_MAX_DEPTH_MAX: u8 = 10;

pub fn default_agent_control_max_depth() -> u8 {
    3
}

pub fn default_auto_generate_agent_names_enabled() -> bool {
    true
}

pub fn default_background_agent_features() -> BackgroundAgentFeaturesSettings {
    BackgroundAgentFeaturesSettings {
        auto_generate_agent_names: true,
        agent_activity_summaries: false,
    }
}

impl Default for BackgroundAgentFeaturesSettings {
    fn default() -> Self {
        default_background_agent_features()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackgroundAgentFeature {
    AutoGenerateAgentNames,
    AgentActivitySummaries,
}

pub type HostBootstrapPayload = protocol::HostBootstrapPayload<HostSettings>;
pub type HostSettingsPayload = protocol::HostSettingsPayload<HostSettings>;

use serde_json::Value;
use sha2::{Digest, Sha256};

/// The build-static JSON Schema (schemars, draft 2020-12) for the host
/// settings document. This is the schema `HostBootstrap` carries and the one
/// the server's generic write path consults for path knowledge and
/// write-only (secret) detection.
pub fn host_settings_schema() -> &'static Value {
    static SCHEMA: OnceLock<Value> = OnceLock::new();
    SCHEMA.get_or_init(|| {
        let mut schema = serde_json::to_value(schemars::schema_for!(HostSettings))
            .expect("host settings schema must serialize");
        decorate_host_settings_schema(&mut schema);
        schema
    })
}

fn decorate_host_settings_schema(schema: &mut Value) {
    let Some(root) = schema.as_object_mut() else {
        return;
    };
    root.insert("x-tyde-ui-version".to_owned(), Value::from(1));
    root.insert(
        "x-tyde-sections".to_owned(),
        serde_json::json!([
            {"id": "general", "title": "General", "order": 10},
            {"id": "subagents", "title": "Subagents", "order": 20},
            {"id": "supervisor", "title": "Supervisor", "order": 30},
            {"id": "voice", "title": "Voice", "order": 40},
            {"id": "mobile", "title": "Mobile", "order": 50}
        ]),
    );

    for (field, section, order, widget) in [
        ("tyde_debug_mcp_enabled", "general", 10, "toggle"),
        ("tyde_agent_control_mcp_enabled", "subagents", 10, "toggle"),
        ("tyde_agent_control_max_depth", "subagents", 20, "slider"),
        ("complexity_tiers_enabled", "subagents", 30, "toggle"),
        ("enable_mobile_connections", "mobile", 10, "toggle"),
        ("mobile_broker_url", "mobile", 20, "text"),
    ] {
        annotate_property(schema, "HostSettings", field, section, order, widget);
    }
    for (field, order, widget) in [("username", 30, "text"), ("password", 40, "password")] {
        annotate_property(
            schema,
            "MobileBrokerAuthSettings",
            field,
            "mobile",
            order,
            widget,
        );
    }
    for (field, title, description) in [
        (
            "tyde_debug_mcp_enabled",
            "Tyde Debug MCP",
            "Start new chats with the Tyde debug MCP server attached, giving agents tools to inspect and drive Tyde's own frontend. Leave this off unless you are working on Tyde itself. Existing chats are unaffected until restarted.",
        ),
        (
            "tyde_agent_control_mcp_enabled",
            "Tyde sub-agents",
            "Allow agents to spawn, message, and await other agents through Tyde's cross-agent orchestration tools. This does not affect agents you create from the UI or a backend's own sub-agent feature.",
        ),
        (
            "tyde_agent_control_max_depth",
            "Maximum agent depth",
            "Count the main task as level 1. Agents at the maximum level cannot create another level. The default of 3 allows the main task, its children, and their children.",
        ),
        (
            "complexity_tiers_enabled",
            "Task complexity tiers",
            "Offer low and high cost configurations when agents are spawned instead of always using the backend default.",
        ),
        (
            "enable_mobile_connections",
            "Mobile connections",
            "Allow paired mobile devices to connect to this host.",
        ),
        (
            "mobile_broker_url",
            "Development broker URL",
            "Optional loopback MQTT broker override used for local development.",
        ),
    ] {
        set_property_text(schema, "HostSettings", field, title, description);
    }
    for (field, title, description) in [
        (
            "username",
            "Development broker username",
            "Username for the loopback development broker override.",
        ),
        (
            "password",
            "Development broker password",
            "Password for the loopback development broker. Stored write-only; the current value is never sent back to clients.",
        ),
    ] {
        set_property_text(
            schema,
            "MobileBrokerAuthSettings",
            field,
            title,
            description,
        );
    }
    for (field, order, widget) in [
        ("enabled", 10, "toggle"),
        ("supervise_restored_agents", 20, "toggle"),
        ("stall_timeout_enabled", 30, "toggle"),
        ("stall_timeout_seconds", 40, "slider"),
        ("auto_compact_on_success", 50, "toggle"),
        ("auto_compact_inactivity_delay_seconds", 60, "slider"),
        ("auto_compact_min_context_tokens", 70, "slider"),
        ("max_kicks_per_task", 80, "slider"),
        ("retry_attempts", 90, "slider"),
        ("cost_tier", 100, "select"),
    ] {
        annotate_property(
            schema,
            "SupervisorSettings",
            field,
            "supervisor",
            order,
            widget,
        );
    }
    for (field, title, description) in [
        (
            "enabled",
            "Enable agent supervisor",
            "When an agent goes idle, run a background verdict that can send a follow-up when the requested work is not finished. This adds a paid model call per idle transition.",
        ),
        (
            "supervise_restored_agents",
            "Supervise restored agents",
            "Judge a restored session as soon as its replayed transcript becomes idle instead of waiting for its first live turn.",
        ),
        (
            "stall_timeout_enabled",
            "Interrupt stalled turns",
            "Cancel a running turn that produces no observable progress for the configured timeout, then let the supervisor decide how to continue.",
        ),
        (
            "stall_timeout_seconds",
            "Stall timeout",
            "Whole seconds without observable turn progress before a stalled turn is interrupted.",
        ),
        (
            "auto_compact_on_success",
            "Auto-compact on success",
            "After the supervisor confirms completion, compact the agent once the inactivity and context thresholds are met.",
        ),
        (
            "auto_compact_inactivity_delay_seconds",
            "Auto-compact inactivity delay",
            "Whole seconds of uninterrupted inactivity required before automatic compaction may start.",
        ),
        (
            "auto_compact_min_context_tokens",
            "Auto-compact minimum context",
            "Minimum reported current-context size in tokens required before automatic compaction.",
        ),
        (
            "max_kicks_per_task",
            "Kick limit",
            "Maximum consecutive supervisor follow-ups without a new user message.",
        ),
        (
            "retry_attempts",
            "Extra delayed attempts",
            "Extra paid attempts after a supervisor verdict call fails or returns an invalid verdict. Zero disables retries.",
        ),
        (
            "cost_tier",
            "Verdict model tier",
            "Model tier used for supervision verdicts: low is cheapest, default uses the backend default, and high uses the most capable configuration.",
        ),
    ] {
        set_property_text(schema, "SupervisorSettings", field, title, description);
    }
    set_numeric_bounds(
        schema,
        "HostSettings",
        "tyde_agent_control_max_depth",
        TYDE_AGENT_CONTROL_MAX_DEPTH_MIN.into(),
        TYDE_AGENT_CONTROL_MAX_DEPTH_MAX.into(),
    );
    set_numeric_bounds(
        schema,
        "SupervisorSettings",
        "stall_timeout_seconds",
        SUPERVISOR_STALL_TIMEOUT_SECONDS_MIN.into(),
        SUPERVISOR_STALL_TIMEOUT_SECONDS_MAX.into(),
    );
    set_numeric_bounds(
        schema,
        "SupervisorSettings",
        "auto_compact_inactivity_delay_seconds",
        SUPERVISOR_AUTO_COMPACT_INACTIVITY_DELAY_SECONDS_MIN.into(),
        SUPERVISOR_AUTO_COMPACT_INACTIVITY_DELAY_SECONDS_MAX.into(),
    );
    set_numeric_bounds(schema, "SupervisorSettings", "max_kicks_per_task", 1, 20);
    set_numeric_bounds(
        schema,
        "SupervisorSettings",
        "retry_attempts",
        SUPERVISOR_RETRY_ATTEMPTS_MIN.into(),
        SUPERVISOR_RETRY_ATTEMPTS_MAX.into(),
    );
    for (field, step) in [
        ("stall_timeout_seconds", 60),
        ("auto_compact_inactivity_delay_seconds", 1),
        ("auto_compact_min_context_tokens", 1_000),
        ("max_kicks_per_task", 1),
        ("retry_attempts", 1),
    ] {
        set_numeric_step(schema, "SupervisorSettings", field, step);
    }
    set_numeric_step(schema, "HostSettings", "tyde_agent_control_max_depth", 1);
}

fn schema_properties_mut<'a>(
    schema: &'a mut Value,
    type_name: &str,
) -> Option<&'a mut serde_json::Map<String, Value>> {
    let node = if type_name == "HostSettings" {
        schema
    } else {
        schema.get_mut("$defs")?.get_mut(type_name)?
    };
    node.get_mut("properties")?.as_object_mut()
}

fn annotate_property(
    schema: &mut Value,
    type_name: &str,
    field: &str,
    section: &str,
    order: u32,
    widget: &str,
) {
    let Some(property) = schema_properties_mut(schema, type_name)
        .and_then(|properties| properties.get_mut(field))
        .and_then(Value::as_object_mut)
    else {
        return;
    };
    property.insert("x-tyde-section".to_owned(), Value::from(section));
    property.insert("x-tyde-order".to_owned(), Value::from(order));
    property.insert("x-tyde-widget".to_owned(), Value::from(widget));
}

fn set_property_text(
    schema: &mut Value,
    type_name: &str,
    field: &str,
    title: &str,
    description: &str,
) {
    let Some(property) = schema_properties_mut(schema, type_name)
        .and_then(|properties| properties.get_mut(field))
        .and_then(Value::as_object_mut)
    else {
        return;
    };
    property.insert("title".to_owned(), Value::from(title));
    property.insert("description".to_owned(), Value::from(description));
}

fn set_numeric_bounds(
    schema: &mut Value,
    type_name: &str,
    field: &str,
    minimum: u64,
    maximum: u64,
) {
    let Some(property) = schema_properties_mut(schema, type_name)
        .and_then(|properties| properties.get_mut(field))
        .and_then(Value::as_object_mut)
    else {
        return;
    };
    property.insert("minimum".to_owned(), Value::from(minimum));
    property.insert("maximum".to_owned(), Value::from(maximum));
}

fn set_numeric_step(schema: &mut Value, type_name: &str, field: &str, step: u64) {
    let Some(property) = schema_properties_mut(schema, type_name)
        .and_then(|properties| properties.get_mut(field))
        .and_then(Value::as_object_mut)
    else {
        return;
    };
    property.insert("multipleOf".to_owned(), Value::from(step));
}

/// Re-export of the wire pointer parser so model helpers and callers share
/// one RFC 6901 implementation.
pub use protocol::types::parse_json_pointer;

/// Escapes one reference token for embedding in an RFC 6901 pointer.
pub fn escape_pointer_token(token: &str) -> String {
    token.replace('~', "~0").replace('/', "~1")
}

/// Joins reference tokens back into an RFC 6901 pointer (`[] -> ""`).
pub fn join_pointer(tokens: &[String]) -> String {
    let mut pointer = String::new();
    for token in tokens {
        pointer.push('/');
        pointer.push_str(&escape_pointer_token(token));
    }
    pointer
}

/// True when one path equals the other or is an ancestor of it.
pub fn paths_overlap(a: &[String], b: &[String]) -> bool {
    let shorter = a.len().min(b.len());
    a[..shorter] == b[..shorter]
}

/// The value at `tokens` in `doc`, if present.
pub fn pointer_get<'doc>(doc: &'doc Value, tokens: &[String]) -> Option<&'doc Value> {
    let mut current = doc;
    for token in tokens {
        current = match current {
            Value::Object(map) => map.get(token)?,
            Value::Array(items) => items.get(token.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(current)
}

/// Sets `value` at `tokens` in `doc`. The parent container must exist; a map
/// member may be created, and an array may grow by exactly one element when
/// the index equals its length. The RFC 6901 `-` append token is rejected:
/// an always-absent target has no meaningful compare-and-swap precondition.
/// Errors name the failing pointer, never a value.
pub fn pointer_set(doc: &mut Value, tokens: &[String], value: Value) -> Result<(), String> {
    let Some((last, parents)) = tokens.split_last() else {
        *doc = value;
        return Ok(());
    };
    let parent = pointer_get_mut(doc, parents)
        .ok_or_else(|| format!("no container exists at {}", join_pointer(parents)))?;
    match parent {
        Value::Object(map) => {
            map.insert(last.clone(), value);
            Ok(())
        }
        Value::Array(items) => {
            let index: usize = last
                .parse()
                .map_err(|_| format!("{} is not an array index", join_pointer(tokens)))?;
            if index < items.len() {
                items[index] = value;
                Ok(())
            } else if index == items.len() {
                items.push(value);
                Ok(())
            } else {
                Err(format!(
                    "array index {} is out of bounds at {}",
                    index,
                    join_pointer(parents)
                ))
            }
        }
        _ => Err(format!("{} is not a container", join_pointer(parents))),
    }
}

/// Removes the member/element at `tokens`. Removing an absent member is a
/// no-op (the mandatory CAS already proved the caller knew it was absent).
pub fn pointer_remove(doc: &mut Value, tokens: &[String]) -> Result<(), String> {
    let Some((last, parents)) = tokens.split_last() else {
        return Err("the whole settings document cannot be removed".to_owned());
    };
    let Some(parent) = pointer_get_mut(doc, parents) else {
        return Ok(());
    };
    match parent {
        Value::Object(map) => {
            map.remove(last);
            Ok(())
        }
        Value::Array(items) => {
            let index: usize = last
                .parse()
                .map_err(|_| format!("{} is not an array index", join_pointer(tokens)))?;
            if index < items.len() {
                items.remove(index);
            }
            Ok(())
        }
        _ => Err(format!("{} is not a container", join_pointer(parents))),
    }
}

fn pointer_get_mut<'doc>(doc: &'doc mut Value, tokens: &[String]) -> Option<&'doc mut Value> {
    let mut current = doc;
    for token in tokens {
        current = match current {
            Value::Object(map) => map.get_mut(token)?,
            Value::Array(items) => items.get_mut(token.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(current)
}

/// The target of `node`'s own `$ref`, if it has one. Outer `None` when the
/// reference cannot be resolved — every caller treats that as *unprovable*
/// and fails closed (unknown for writes, redacted for outbound documents).
fn ref_target<'schema>(
    root: &'schema Value,
    node: &'schema Value,
) -> Option<Option<&'schema Value>> {
    let Some(reference) = node.get("$ref").and_then(Value::as_str) else {
        return Some(None);
    };
    let path = reference.strip_prefix("#/")?;
    let mut current = root;
    for raw in path.split('/') {
        let token = raw.replace("~1", "/").replace("~0", "~");
        current = current.get(&token)?;
    }
    Some(Some(current))
}

fn subschema_branches(node: &Value) -> Vec<&Value> {
    ["anyOf", "oneOf", "allOf"]
        .iter()
        .filter_map(|key| node.get(*key).and_then(Value::as_array))
        .flatten()
        .collect()
}

fn node_write_only(node: &Value) -> bool {
    node.get("writeOnly").and_then(Value::as_bool) == Some(true)
}

/// Expands one schema position into the set of nodes that can describe an
/// instance there: the node itself, its `$ref` targets, and every
/// `anyOf`/`oneOf`/`allOf` branch, recursively. Referencing nodes are kept
/// in the set alongside their targets so sibling keywords (e.g. a
/// field-level `writeOnly` next to a `$ref`) are never lost. `None` when
/// any reference is unresolvable — the position is then *unprovable* and
/// every caller fails closed.
fn expand_schema<'schema>(
    root: &'schema Value,
    node: &'schema Value,
) -> Option<Vec<&'schema Value>> {
    let mut out: Vec<&Value> = Vec::new();
    let mut stack = vec![node];
    let mut seen: Vec<*const Value> = Vec::new();
    while let Some(next) = stack.pop() {
        let key = next as *const Value;
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        out.push(next);
        if let Some(target) = ref_target(root, next)? {
            stack.push(target);
        }
        stack.extend(subschema_branches(next));
    }
    Some(out)
}

/// The candidate schemas for member `token` of an object instance whose
/// position is described by the expanded node set `nodes`. Union semantics:
/// a member declared by *any* node (its `properties`, or an object-valued
/// `additionalProperties` for map types) is known through that declaration.
/// `None` means the member is *unprovable* — `additionalProperties: true`,
/// a bool schema, or an unresolvable child — and callers fail closed.
fn member_candidates<'schema>(
    root: &'schema Value,
    nodes: &[&'schema Value],
    token: &str,
) -> Option<Vec<&'schema Value>> {
    let mut candidates: Vec<&Value> = Vec::new();
    for node in nodes {
        if node.as_bool().is_some() {
            // A bool schema proves nothing about the member's shape.
            return None;
        }
        if let Some(child) = node
            .get("properties")
            .and_then(Value::as_object)
            .and_then(|properties| properties.get(token))
        {
            candidates.extend(expand_schema(root, child)?);
            continue;
        }
        match node.get("additionalProperties") {
            Some(child @ Value::Object(_)) => candidates.extend(expand_schema(root, child)?),
            Some(Value::Bool(true)) => return None,
            _ => {}
        }
    }
    Some(candidates)
}

/// The candidate schemas for an array element whose array position is
/// described by `nodes`. `None` when any array-typed node lacks a provable
/// `items` schema.
fn item_candidates<'schema>(
    root: &'schema Value,
    nodes: &[&'schema Value],
) -> Option<Vec<&'schema Value>> {
    let mut candidates: Vec<&Value> = Vec::new();
    let mut saw_array = false;
    for node in nodes {
        if node.as_bool().is_some() {
            return None;
        }
        if schema_declares_array(node) {
            saw_array = true;
            candidates.extend(expand_schema(root, node.get("items")?)?);
        } else if node.get("type").is_none()
            && node.get("$ref").is_none()
            && subschema_branches(node).is_empty()
        {
            return None;
        }
    }
    saw_array.then_some(candidates)
}

fn schema_declares_array(node: &Value) -> bool {
    match node.get("type") {
        Some(Value::String(kind)) => kind == "array",
        Some(Value::Array(kinds)) => kinds.iter().any(|kind| kind.as_str() == Some("array")),
        _ => node.get("items").is_some(),
    }
}

/// True when the settings schema knows a value can exist at `tokens`:
/// declared struct fields, map members under an object-valued
/// `additionalProperties`, and array elements under `items`. Fail-closed: a
/// key absent from every candidate's `properties`, an `additionalProperties:
/// true`, a bool schema, or an unresolvable reference are all *unknown* —
/// serde would silently drop such members, and that silence is exactly what
/// this explicit check exists to prevent. The RFC 6901 `-` append token is
/// never known (see `pointer_set`).
pub fn schema_knows_path(schema: &Value, tokens: &[String]) -> bool {
    let Some(mut nodes) = expand_schema(schema, schema) else {
        return false;
    };
    for token in tokens {
        let object_candidates = match member_candidates(schema, &nodes, token) {
            Some(candidates) => candidates,
            None => return false,
        };
        let mut next = object_candidates;
        if token.parse::<usize>().is_ok() {
            match item_candidates(schema, &nodes) {
                Some(candidates) => next.extend(candidates),
                None => return false,
            }
        }
        if next.is_empty() {
            return false;
        }
        nodes = next;
    }
    true
}

/// How a pointer relates to write-only (secret) schema regions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SecretRelation {
    /// The target sits at or inside a `writeOnly` subtree. Writes here are
    /// legitimate (that is how a secret is set/cleared) but value-carrying
    /// expectations are not: the current value is never on the wire.
    pub at_or_inside_secret: bool,
    /// The target is a strict ancestor of a `writeOnly` subtree. Replacing
    /// it wholesale could silently clear omitted secret values, so such
    /// writes are rejected.
    pub strict_ancestor_of_secret: bool,
}

/// Computes the [`SecretRelation`] of `tokens` under `schema`. Fail-closed:
/// an unprovable position (unresolvable reference, bool schema,
/// `additionalProperties: true`) is treated as secret-bearing; such paths
/// are additionally unknown per [`schema_knows_path`], which rejects the
/// write first.
pub fn secret_relation(schema: &Value, tokens: &[String]) -> SecretRelation {
    let sealed = SecretRelation {
        at_or_inside_secret: true,
        strict_ancestor_of_secret: true,
    };
    let Some(mut nodes) = expand_schema(schema, schema) else {
        return sealed;
    };
    let mut inherited_secret = false;
    for token in tokens {
        inherited_secret |= nodes.iter().copied().any(node_write_only);
        let mut next = match member_candidates(schema, &nodes, token) {
            Some(candidates) => candidates,
            None => return sealed,
        };
        if token.parse::<usize>().is_ok() {
            match item_candidates(schema, &nodes) {
                Some(candidates) => next.extend(candidates),
                None => return sealed,
            }
        }
        if next.is_empty() {
            return sealed;
        }
        nodes = next;
    }
    let at_target_secret = nodes.iter().copied().any(node_write_only);
    if inherited_secret || at_target_secret {
        return SecretRelation {
            at_or_inside_secret: true,
            strict_ancestor_of_secret: false,
        };
    }
    // Unprovable below the target fails closed as ancestor-of-secret.
    let contains_secret_below = nodes
        .iter()
        .any(|node| contains_write_only(schema, node, &mut Vec::new()).unwrap_or(true));
    SecretRelation {
        at_or_inside_secret: false,
        strict_ancestor_of_secret: contains_secret_below,
    }
}

/// Whether any `writeOnly` subschema exists at or below `node`. `None` when
/// the subtree contains an unresolvable reference (unprovable).
fn contains_write_only<'schema>(
    root: &'schema Value,
    node: &'schema Value,
    visiting: &mut Vec<*const Value>,
) -> Option<bool> {
    let key = node as *const Value;
    if visiting.contains(&key) {
        return Some(false);
    }
    visiting.push(key);
    let mut found = node_write_only(node);
    if !found {
        match ref_target(root, node) {
            Some(Some(target)) => {
                if contains_write_only(root, target, visiting)? {
                    found = true;
                }
            }
            Some(None) => {}
            None => {
                visiting.pop();
                return None;
            }
        }
    }
    if !found && let Some(properties) = node.get("properties").and_then(Value::as_object) {
        for child in properties.values() {
            if contains_write_only(root, child, visiting)? {
                found = true;
                break;
            }
        }
    }
    if !found
        && let Some(child @ Value::Object(_)) = node.get("additionalProperties")
        && contains_write_only(root, child, visiting)?
    {
        found = true;
    }
    if !found
        && let Some(items) = node.get("items")
        && contains_write_only(root, items, visiting)?
    {
        found = true;
    }
    if !found {
        for branch in subschema_branches(node) {
            if contains_write_only(root, branch, visiting)? {
                found = true;
                break;
            }
        }
    }
    visiting.pop();
    Some(found)
}

/// What to do with one object member during redaction.
enum MemberAction<'schema> {
    Recurse(Vec<&'schema Value>),
    /// The member is a write-only value (or an array of write-only
    /// elements): record it as configured when non-null and remove it.
    Secret,
    /// The member's schema cannot be proved safe (undeclared key,
    /// `additionalProperties: true`, bool schema, unresolvable reference).
    /// Fail closed: remove it from the outbound document without claiming
    /// it is a configured secret.
    Drop,
}

fn array_element_action<'schema>(
    root: &'schema Value,
    nodes: &[&'schema Value],
    visiting: &mut Vec<*const Value>,
) -> MemberAction<'schema> {
    let Some(items) = item_candidates(root, nodes) else {
        return MemberAction::Drop;
    };
    if items.is_empty() {
        return MemberAction::Drop;
    }
    if items.iter().copied().any(node_write_only) {
        return MemberAction::Secret;
    }
    for item in &items {
        if !schema_declares_array(item) {
            continue;
        }
        let key = *item as *const Value;
        if visiting.contains(&key) {
            return MemberAction::Drop;
        }
        visiting.push(key);
        let nested = array_element_action(root, &items, visiting);
        visiting.pop();
        if !matches!(&nested, MemberAction::Recurse(_)) {
            return nested;
        }
    }
    MemberAction::Recurse(items)
}

fn member_action<'schema>(
    root: &'schema Value,
    nodes: &[&'schema Value],
    token: &str,
    instance: &Value,
) -> MemberAction<'schema> {
    let Some(candidates) = member_candidates(root, nodes, token) else {
        return MemberAction::Drop;
    };
    if candidates.is_empty() {
        return MemberAction::Drop;
    }
    if candidates.iter().copied().any(node_write_only) {
        return MemberAction::Secret;
    }
    if instance.is_array() {
        match array_element_action(root, &candidates, &mut Vec::new()) {
            MemberAction::Recurse(_) => {}
            action => return action,
        }
    }
    MemberAction::Recurse(candidates)
}

/// Removes every write-only (secret) value from `doc`, walking the schema
/// and instance together, and returns each removed pointer with the true
/// value it held (never serialized — the server derives value-free version
/// tokens from it). Fail-closed: an instance member whose schema cannot be
/// resolved and proved safe is removed from the outbound document.
pub fn redact_write_only(schema: &Value, doc: &mut Value) -> Vec<(String, Value)> {
    let mut configured = Vec::new();
    match expand_schema(schema, schema) {
        Some(nodes) => redact_walk(schema, &nodes, doc, &mut Vec::new(), &mut configured),
        None => {
            // The whole schema is unprovable; publish nothing.
            *doc = Value::Null;
        }
    }
    configured
}

fn redact_walk(
    root: &Value,
    nodes: &[&Value],
    instance: &mut Value,
    path: &mut Vec<String>,
    configured: &mut Vec<(String, Value)>,
) {
    match instance {
        Value::Object(map) => {
            let keys: Vec<String> = map.keys().cloned().collect();
            for key in keys {
                let action = member_action(root, nodes, &key, &map[&key]);
                match action {
                    MemberAction::Recurse(candidates) => {
                        path.push(key.clone());
                        redact_walk(
                            root,
                            &candidates,
                            map.get_mut(&key).expect("member exists"),
                            path,
                            configured,
                        );
                        path.pop();
                    }
                    MemberAction::Secret => {
                        let value = map.remove(&key).expect("member exists");
                        path.push(key);
                        if !value.is_null() {
                            configured.push((join_pointer(path), value));
                        }
                        path.pop();
                    }
                    MemberAction::Drop => {
                        map.remove(&key);
                    }
                }
            }
        }
        Value::Array(items) => match array_element_action(root, nodes, &mut Vec::new()) {
            MemberAction::Recurse(candidates) => {
                for (index, item) in items.iter_mut().enumerate() {
                    path.push(index.to_string());
                    redact_walk(root, &candidates, item, path, configured);
                    path.pop();
                }
            }
            MemberAction::Secret => {
                let value = Value::Array(std::mem::take(items));
                if !value.as_array().is_some_and(Vec::is_empty) {
                    configured.push((join_pointer(path), value));
                }
            }
            MemberAction::Drop => items.clear(),
        },
        _ => {}
    }
}

/// Canonical JSON serialization: object keys sorted, no whitespace. The
/// basis for [`version_token`] so tokens are stable across processes and
/// serializer configurations.
pub fn canonical_json(value: &Value) -> String {
    let mut out = String::new();
    write_canonical(value, &mut out);
    out
}

fn write_canonical(value: &Value, out: &mut String) {
    match value {
        Value::Object(map) => {
            out.push('{');
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            for (index, key) in keys.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::to_string(key).expect("string serializes"));
                out.push(':');
                write_canonical(&map[key.as_str()], out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        other => out.push_str(&serde_json::to_string(other).expect("scalar serializes")),
    }
}

/// Value-free version token for a (client-visible) subtree: hex SHA-256 of
/// its canonical JSON. An absent subtree tokens identically to an explicit
/// `null`, mirroring the value-expectation rule.
pub fn version_token(value: Option<&Value>) -> String {
    let canonical = canonical_json(value.unwrap_or(&Value::Null));
    let digest = Sha256::digest(canonical.as_bytes());
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Server-issued, value-free revision token for one secret-bearing path:
/// HMAC-SHA256 over the pointer, a presence marker, and the canonical JSON
/// of the TRUE (unredacted) value, keyed with a random per-host key that
/// persists beside the settings store. It changes whenever the secret
/// changes, is stable across restarts, distinguishes absent from every
/// configured value, and cannot be derived from a guessed secret without
/// the key. Both sides of the wire only ever see the token.
pub fn secret_version_token(key: &[u8], pointer: &str, value: Option<&Value>) -> String {
    use hmac::{Hmac, Mac};
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC-SHA256 accepts any key length");
    mac.update(pointer.as_bytes());
    match value {
        Some(value) => {
            mac.update(&[0x01]);
            mac.update(canonical_json(value).as_bytes());
        }
        None => mac.update(&[0x00]),
    }
    hex_digest(&mac.finalize().into_bytes())
}

fn hex_digest(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// The etag published with every outbound settings document: a hash of the
/// client-visible (secret-redacted) document PLUS the configured-secret
/// revision tokens, so a secret-only change still advances the etag.
/// Content-derived (with the persistent secret-token key), so restarts
/// cannot mint spurious conflicts.
pub fn document_etag(
    redacted_doc: &Value,
    configured_secrets: &[protocol::types::ConfiguredSecret],
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(canonical_json(redacted_doc).as_bytes());
    let mut secrets: Vec<(&str, &str)> = configured_secrets
        .iter()
        .map(|secret| (secret.pointer.as_str(), secret.token.as_str()))
        .collect();
    secrets.sort();
    for (pointer, token) in secrets {
        hasher.update([0x00]);
        hasher.update(pointer.as_bytes());
        hasher.update([0x00]);
        hasher.update(token.as_bytes());
    }
    hex_digest(&hasher.finalize())
}
