use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::str::FromStr;

use schemars::JsonSchema;
use serde::de::{DeserializeOwned, Error as DeError};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

/// Prerelease-capable, traversal-safe release identifier used as the versioned
/// bundle key for the web/PWA client. Single source of truth lives in
/// `host-config`; re-exported here so wire payloads and downstream crates use
/// `protocol::TydeReleaseVersion`.
pub use host_config::{LOCAL_HOST_ID, TydeReleaseVersion};

pub const PROTOCOL_VERSION: u32 = 38;
pub const TYDE_VERSION: Version = Version {
    major: 0,
    minor: 8,
    patch: 14,
};
/// Shared MQTT-over-WebSocket-Secure endpoint reachable from both the native
/// host and the browser/PWA client (no mixed content; broker terminates TLS).
pub const DEFAULT_MOBILE_MQTT_BROKER_URL: &str = "wss://broker.emqx.io:8084/mqtt";
pub const DEFAULT_SESSION_LIST_PAGE_LIMIT: u32 = 64;
pub const DEFAULT_MOBILE_SESSION_LIST_PAGE_LIMIT: u32 = 20;
pub const MAX_SESSION_LIST_PAGE_LIMIT: u32 = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

impl FromStr for Version {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let trimmed = value.strip_prefix('v').unwrap_or(value);
        let mut parts = trimmed.split('.');
        let major = parse_version_component(parts.next(), "major")?;
        let minor = parse_version_component(parts.next(), "minor")?;
        let patch = parse_version_component(parts.next(), "patch")?;
        if parts.next().is_some() {
            return Err(format!("version has too many components: {value}"));
        }
        Ok(Self {
            major,
            minor,
            patch,
        })
    }
}

fn parse_version_component(component: Option<&str>, name: &str) -> Result<u32, String> {
    let component = component.ok_or_else(|| format!("version is missing {name} component"))?;
    if component.is_empty() {
        return Err(format!("version has empty {name} component"));
    }
    component
        .parse::<u32>()
        .map_err(|err| format!("invalid {name} version component {component:?}: {err}"))
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct StreamPath(pub String);

impl fmt::Display for StreamPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolTypeError {
    EmptyIdentifier { type_name: &'static str },
}

impl fmt::Display for ProtocolTypeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyIdentifier { type_name } => {
                write!(f, "{type_name} must not be empty")
            }
        }
    }
}

impl std::error::Error for ProtocolTypeError {}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BrokerUrl(String);

impl BrokerUrl {
    pub fn new(value: impl Into<String>) -> Result<Self, ProtocolTypeError> {
        let value = value.into();
        if value.is_empty() {
            return Err(ProtocolTypeError::EmptyIdentifier {
                type_name: "BrokerUrl",
            });
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for BrokerUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ManagedBrokerRegion(String);

impl ManagedBrokerRegion {
    pub fn new(value: impl Into<String>) -> Result<Self, ProtocolTypeError> {
        let value = value.into();
        if value.is_empty() {
            return Err(ProtocolTypeError::EmptyIdentifier {
                type_name: "ManagedBrokerRegion",
            });
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ManagedBrokerRegion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for ManagedBrokerRegion {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ManagedBrokerRegion {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ManagedBrokerAuthorizerName(String);

impl ManagedBrokerAuthorizerName {
    pub fn new(value: impl Into<String>) -> Result<Self, ProtocolTypeError> {
        let value = value.into();
        if value.is_empty() {
            return Err(ProtocolTypeError::EmptyIdentifier {
                type_name: "ManagedBrokerAuthorizerName",
            });
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ManagedBrokerAuthorizerName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for ManagedBrokerAuthorizerName {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ManagedBrokerAuthorizerName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ManagedBrokerGrantId(String);

impl ManagedBrokerGrantId {
    pub fn new(value: impl Into<String>) -> Result<Self, ProtocolTypeError> {
        let value = value.into();
        if value.is_empty() {
            return Err(ProtocolTypeError::EmptyIdentifier {
                type_name: "ManagedBrokerGrantId",
            });
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ManagedBrokerGrantId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for ManagedBrokerGrantId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ManagedBrokerGrantId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ManagedBrokerClientId(String);

impl ManagedBrokerClientId {
    pub fn new(value: impl Into<String>) -> Result<Self, ProtocolTypeError> {
        let value = value.into();
        if value.is_empty() {
            return Err(ProtocolTypeError::EmptyIdentifier {
                type_name: "ManagedBrokerClientId",
            });
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ManagedBrokerClientId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for ManagedBrokerClientId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ManagedBrokerClientId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ManagedBrokerTopicNamespace(String);

impl ManagedBrokerTopicNamespace {
    pub fn new(value: impl Into<String>) -> Result<Self, ProtocolTypeError> {
        let value = value.into();
        if value.is_empty() {
            return Err(ProtocolTypeError::EmptyIdentifier {
                type_name: "ManagedBrokerTopicNamespace",
            });
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ManagedBrokerTopicNamespace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for ManagedBrokerTopicNamespace {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ManagedBrokerTopicNamespace {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MobilePairingOfferId(pub String);

impl MobilePairingOfferId {
    pub fn new(value: impl Into<String>) -> Result<Self, ProtocolTypeError> {
        let value = value.into();
        if value.is_empty() {
            return Err(ProtocolTypeError::EmptyIdentifier {
                type_name: "MobilePairingOfferId",
            });
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for MobilePairingOfferId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for MobilePairingOfferId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for MobilePairingOfferId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MobileDeviceId(pub String);

impl fmt::Display for MobileDeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MobilePairingQrUri(pub String);

impl fmt::Display for MobilePairingQrUri {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Strongly typed agent identifier. Wraps a UUID string.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentId(pub String);

impl fmt::Display for AgentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(pub String);

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct QueuedMessageId(pub String);

impl fmt::Display for QueuedMessageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ChatMessageId(pub String);

impl fmt::Display for ChatMessageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct ReviewId(pub String);

impl fmt::Display for ReviewId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ReviewCommentId(pub String);

impl fmt::Display for ReviewCommentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ReviewSuggestionId(pub String);

impl fmt::Display for ReviewSuggestionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct ProjectId(pub String);

impl fmt::Display for ProjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CustomAgentId(pub String);

impl fmt::Display for CustomAgentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TeamId(pub String);

impl fmt::Display for TeamId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct WorkflowId(pub String);

impl fmt::Display for WorkflowId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WorkflowRunId(pub String);

impl fmt::Display for WorkflowRunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WorkflowStepRunId(pub String);

impl fmt::Display for WorkflowStepRunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TeamMemberId(pub String);

impl fmt::Display for TeamMemberId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TeamDraftId(pub String);

impl fmt::Display for TeamDraftId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TeamDraftMemberId(pub String);

impl fmt::Display for TeamDraftMemberId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TeamRolePresetId(pub String);

impl fmt::Display for TeamRolePresetId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TeamPersonalityPresetId(pub String);

impl fmt::Display for TeamPersonalityPresetId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TeamTemplateId(pub String);

impl fmt::Display for TeamTemplateId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SteeringId(pub String);

impl fmt::Display for SteeringId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SkillId(pub String);

impl fmt::Display for SkillId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct McpServerId(pub String);

impl fmt::Display for McpServerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Which coding agent backend to use. Enum, not string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BackendKind {
    Tycode,
    Kiro,
    Claude,
    Codex,
    Antigravity,
    Hermes,
}

impl BackendKind {
    pub const fn supports_image_input(self) -> bool {
        match self {
            Self::Kiro | Self::Claude | Self::Codex => true,
            Self::Tycode | Self::Antigravity | Self::Hermes => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct LaunchProfileId(pub String);

impl fmt::Display for LaunchProfileId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchProfileCatalog {
    #[serde(default)]
    pub entries: Vec<LaunchProfileEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_profile_id: Option<LaunchProfileId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LaunchProfileKind {
    BackendDefault,
    Custom,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchProfile {
    pub id: LaunchProfileId,
    pub kind: LaunchProfileKind,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub backend_kind: BackendKind,
    #[serde(default)]
    pub session_settings: SessionSettingsValues,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum LaunchProfileEntry {
    Ready {
        profile: LaunchProfile,
    },
    Unavailable {
        id: LaunchProfileId,
        kind: LaunchProfileKind,
        backend_kind: BackendKind,
        label: String,
        message: String,
    },
}

impl LaunchProfileEntry {
    pub fn id(&self) -> &LaunchProfileId {
        match self {
            Self::Ready { profile } => &profile.id,
            Self::Unavailable { id, .. } => id,
        }
    }

    pub fn backend_kind(&self) -> BackendKind {
        match self {
            Self::Ready { profile } => profile.backend_kind,
            Self::Unavailable { backend_kind, .. } => *backend_kind,
        }
    }

    pub fn kind(&self) -> LaunchProfileKind {
        match self {
            Self::Ready { profile } => profile.kind,
            Self::Unavailable { kind, .. } => *kind,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchProfileCatalogPayload {
    pub catalog: LaunchProfileCatalog,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostLaunchProfileConfig {
    pub id: LaunchProfileId,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub backend_kind: BackendKind,
    #[serde(default)]
    pub session_settings: SessionSettingsValues,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BackendAccessMode {
    #[default]
    Unrestricted,
    /// Backend MUST refuse to execute any tool that mutates the
    /// filesystem, runs shell commands, or otherwise changes state
    /// outside the agent's own message stream. Read-only filesystem
    /// access (read files, list directories, glob, grep) and
    /// configured MCP tool calls are still allowed. The exact
    /// implementation depends on the backend's available knobs.
    ReadOnly,
}

/// Provenance of a live agent — who created it.
/// `parent_agent_id` answers "which agent owns this child"; `origin` answers "who created it."
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentOrigin {
    /// Explicitly spawned or resumed by a human user.
    User,
    /// Spawned programmatically through Tyde-owned orchestration (e.g. agent-control MCP).
    AgentControl,
    /// Spawned as a first-class fork of an existing session for a side question.
    SideQuestion,
    /// Spawned by the backend's own native sub-agent mechanism (e.g. Claude subagents).
    BackendNative,
    /// Spawned as a persistent member of a server-owned agent team.
    TeamMember,
    /// Spawned by a Tyde Workflow coordinator or by a workflow coordinator via MCP.
    Workflow,
}

/// Tool-visible status for agent-control MCP responses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentControlStatus {
    Thinking,
    Idle,
    Failed,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentControlOutput {
    #[default]
    Empty,
    Message {
        text: String,
    },
    Error {
        error: AgentErrorPayload,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentControlOutputProjectionError {
    InvalidAgentError(String),
    InvalidChatEvent(String),
    EventLogRewound { observed: usize, actual: usize },
}

impl fmt::Display for AgentControlOutputProjectionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidAgentError(error) => write!(f, "invalid agent error output: {error}"),
            Self::InvalidChatEvent(error) => write!(f, "invalid chat output event: {error}"),
            Self::EventLogRewound { observed, actual } => write!(
                f,
                "agent output event log rewound from {observed} observed records to {actual}"
            ),
        }
    }
}

impl std::error::Error for AgentControlOutputProjectionError {}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentControlLatestOutput {
    output: AgentControlOutput,
    observed_records: usize,
}

impl AgentControlLatestOutput {
    pub fn output(&self) -> &AgentControlOutput {
        &self.output
    }

    pub fn replace_from_bootstrap(&mut self, output: AgentControlOutput) {
        self.output = output;
    }

    pub fn observe_envelope(
        &mut self,
        envelope: &Envelope,
    ) -> Result<(), AgentControlOutputProjectionError> {
        if let Some(output) = agent_control_output_from_envelope(envelope)? {
            self.output = output;
        }
        Ok(())
    }

    pub fn observe_event_log(
        &mut self,
        event_log: &[Envelope],
    ) -> Result<(), AgentControlOutputProjectionError> {
        if event_log.len() < self.observed_records {
            return Err(AgentControlOutputProjectionError::EventLogRewound {
                observed: self.observed_records,
                actual: event_log.len(),
            });
        }
        for envelope in &event_log[self.observed_records..] {
            self.observe_envelope(envelope)?;
        }
        self.observed_records = event_log.len();
        Ok(())
    }
}

pub fn agent_control_output_from_envelope(
    envelope: &Envelope,
) -> Result<Option<AgentControlOutput>, AgentControlOutputProjectionError> {
    match envelope.kind {
        FrameKind::AgentError => envelope
            .parse_payload::<AgentErrorPayload>()
            .map(|error| Some(AgentControlOutput::Error { error }))
            .map_err(|error| {
                AgentControlOutputProjectionError::InvalidAgentError(error.to_string())
            }),
        FrameKind::ChatEvent => envelope
            .parse_payload::<ChatEvent>()
            .map(|event| agent_control_output_from_chat_event(&event))
            .map_err(|error| {
                AgentControlOutputProjectionError::InvalidChatEvent(error.to_string())
            }),
        _ => Ok(None),
    }
}

pub fn agent_control_output_from_chat_event(event: &ChatEvent) -> Option<AgentControlOutput> {
    let message = match event {
        ChatEvent::MessageAdded(message) => message,
        ChatEvent::StreamEnd(data) => &data.message,
        _ => return None,
    };
    if !matches!(message.sender, MessageSender::Assistant { .. }) {
        return None;
    }
    if message.content.trim().is_empty() {
        Some(AgentControlOutput::Empty)
    } else {
        Some(AgentControlOutput::Message {
            text: message.content.clone(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentControlReadResult {
    pub agent_id: AgentId,
    pub output: AgentControlOutput,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentControlReadDebugResult {
    pub agent_id: AgentId,
    pub events: Vec<Envelope>,
    pub next_after_seq: Option<u64>,
    pub max_bytes: usize,
    pub omitted_events: usize,
    pub omitted_event_bytes: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AgentControlCappedEvents {
    pub events: Vec<Envelope>,
    pub next_after_seq: Option<u64>,
    pub omitted_events: usize,
    pub omitted_event_bytes: usize,
}

pub const AGENT_CONTROL_DEFAULT_READ_LIMIT: usize = 50;
pub const AGENT_CONTROL_MAX_READ_LIMIT: usize = 200;
pub const AGENT_CONTROL_DEFAULT_READ_MAX_BYTES: usize = 256 * 1024;
pub const AGENT_CONTROL_MAX_READ_MAX_BYTES: usize = 1024 * 1024;

pub fn cap_agent_control_events(
    events: Vec<Envelope>,
    max_bytes: usize,
    after_seq: Option<u64>,
) -> Result<AgentControlCappedEvents, serde_json::Error> {
    let mut kept = Vec::new();
    let mut used_bytes = 0usize;
    let mut omitted_events = 0usize;
    let mut omitted_event_bytes = 0usize;
    let mut next_after_seq = after_seq;

    for event in events {
        let event_bytes = serde_json::to_vec(&event)?.len();
        next_after_seq = Some(event.seq);
        if used_bytes.saturating_add(event_bytes) <= max_bytes {
            used_bytes = used_bytes.saturating_add(event_bytes);
            kept.push(event);
        } else {
            omitted_events = omitted_events.saturating_add(1);
            omitted_event_bytes = omitted_event_bytes.saturating_add(event_bytes);
        }
    }

    Ok(AgentControlCappedEvents {
        events: kept,
        next_after_seq,
        omitted_events,
        omitted_event_bytes,
    })
}

/// Backend-agnostic hint for picking a cheaper or more capable spawned agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpawnCostHint {
    Low,
    #[serde(rename = "med", alias = "medium")]
    Medium,
    High,
}

/// Machine-readable agent error categories.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentErrorCode {
    BackendFailed,
    Internal,
    Unsupported,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientErrorCode {
    ProtocolParse,
    ProtocolValidation,
    Transport,
    Internal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrameKind {
    // Handshake
    Hello,
    Welcome,
    Reject,

    // Input events (client -> server)
    SetSetting,
    SetAgentsViewPreferences,
    SetAgentsSmartViews,
    SetAgentTags,
    SetAgentPins,
    SetAgentGroups,
    SpawnAgent,
    LoadAgent,
    FetchSessionHistory,
    ListSessions,
    DeleteSession,
    SendMessage,
    EditQueuedMessage,
    CancelQueuedMessage,
    SendQueuedMessageNow,
    SetAgentName,
    AgentCompact,
    Interrupt,
    CloseAgent,
    RunBackendSetup,
    ProjectCreate,
    ProjectRename,
    ProjectReorder,
    ProjectAddRoot,
    ProjectDeleteRoot,
    ProjectDelete,
    WorkbenchCreate,
    WorkbenchRemove,
    CustomAgentUpsert,
    CustomAgentDelete,
    SteeringUpsert,
    SteeringDelete,
    SkillRefresh,
    McpServerUpsert,
    McpServerDelete,
    TeamCreate,
    TeamRename,
    TeamDelete,
    TeamSetManager,
    TeamMemberCreate,
    TeamMemberUpdate,
    TeamMemberDelete,
    TeamMemberActivate,
    TeamCompact,
    TeamMemberShuffle,
    TeamDraftCreate,
    TeamDraftUpdate,
    TeamDraftShuffle,
    TeamDraftApplyTemplate,
    TeamDraftCommit,
    TeamDraftDiscard,
    ProjectReadDiff,
    ProjectReadFile,
    ProjectSearch,
    ProjectSearchCancel,
    ProjectAccessed,
    CodeIntelSubscribeFile,
    CodeIntelUnsubscribeFile,
    CodeIntelSetVisibleRange,
    CodeIntelHover,
    CodeIntelNavigate,
    CodeIntelFindReferences,
    CodeIntelCancelReferences,
    ProjectStageFile,
    ProjectStageHunk,
    ProjectUnstageFile,
    ProjectDiscardFile,
    ProjectGitCommit,
    ProjectListDir,
    HostBrowseStart,
    HostBrowseList,
    HostBrowseClose,
    TerminalCreate,
    TerminalSend,
    TerminalResize,
    TerminalClose,
    MobilePairingStart,
    MobilePairingCancel,
    MobileDeviceRevoke,
    MobileDeviceRename,
    ClientError,
    Heartbeat,

    SetSessionSettings,
    TriggerWorkflow,
    CancelWorkflow,
    WorkflowRefresh,

    // Output events (server -> client)
    HostBootstrap,
    AgentBootstrap,
    ProjectBootstrap,
    ReviewBootstrap,
    BrowseBootstrap,
    TerminalBootstrap,
    HostSettings,
    AgentsViewPreferencesNotify,
    BackendSetup,
    NewAgent,
    AgentActivitySummary,
    AgentActivityStats,
    TaskTokenUsage,
    AgentStart,
    AgentRenamed,
    AgentCompactNotify,
    AgentClosed,
    ChatEvent,
    SessionHistory,
    AgentError,
    QueuedMessages,
    SessionList,
    ProjectNotify,
    CustomAgentNotify,
    SteeringNotify,
    SkillNotify,
    McpServerNotify,
    TeamNotify,
    TeamMemberNotify,
    TeamMemberBindingNotify,
    TeamCompactNotify,
    TeamPresetCatalogNotify,
    TeamDraftNotify,
    TeamMemberShuffleSuggestionNotify,
    ProjectFileList,
    ProjectGitStatus,
    ProjectFileContents,
    ProjectSearchResults,
    ProjectSearchComplete,
    CodeIntelOverview,
    CodeIntelStatus,
    CodeIntelFileModel,
    CodeIntelDiagnostics,
    CodeIntelHoverResult,
    CodeIntelNavigateResult,
    CodeIntelReferencesResults,
    CodeIntelReferencesComplete,
    CodeIntelError,
    ProjectGitDiff,
    ProjectGitCommitResult,
    NewTerminal,
    TerminalStart,
    TerminalOutput,
    TerminalExit,
    TerminalError,
    HostBrowseOpened,
    HostBrowseEntries,
    HostBrowseError,
    CommandError,
    SessionSchemas,
    SessionSettings,
    BackendConfigSchemas,
    BackendConfigSnapshots,
    BackendCapacity,
    LaunchProfileCatalogNotify,
    MobileAccessState,
    MobilePairingOffer,
    ReviewCreate,
    ReviewAction,
    ReviewEvent,
    ReviewSubscribe,
    ProjectEvent,
    WorkflowNotify,
    WorkflowRunNotify,
    HeartbeatAck,
}

impl fmt::Display for FrameKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Hello => f.write_str("hello"),
            Self::Welcome => f.write_str("welcome"),
            Self::Reject => f.write_str("reject"),
            Self::SetSetting => f.write_str("set_setting"),
            Self::SetAgentsViewPreferences => f.write_str("set_agents_view_preferences"),
            Self::SetAgentsSmartViews => f.write_str("set_agents_smart_views"),
            Self::SetAgentTags => f.write_str("set_agent_tags"),
            Self::SetAgentPins => f.write_str("set_agent_pins"),
            Self::SetAgentGroups => f.write_str("set_agent_groups"),
            Self::SpawnAgent => f.write_str("spawn_agent"),
            Self::LoadAgent => f.write_str("load_agent"),
            Self::FetchSessionHistory => f.write_str("fetch_session_history"),
            Self::ListSessions => f.write_str("list_sessions"),
            Self::DeleteSession => f.write_str("delete_session"),
            Self::SendMessage => f.write_str("send_message"),
            Self::EditQueuedMessage => f.write_str("edit_queued_message"),
            Self::CancelQueuedMessage => f.write_str("cancel_queued_message"),
            Self::SendQueuedMessageNow => f.write_str("send_queued_message_now"),
            Self::SetAgentName => f.write_str("set_agent_name"),
            Self::AgentCompact => f.write_str("agent_compact"),
            Self::Interrupt => f.write_str("interrupt"),
            Self::CloseAgent => f.write_str("close_agent"),
            Self::RunBackendSetup => f.write_str("run_backend_setup"),
            Self::ProjectCreate => f.write_str("project_create"),
            Self::ProjectRename => f.write_str("project_rename"),
            Self::ProjectReorder => f.write_str("project_reorder"),
            Self::ProjectAddRoot => f.write_str("project_add_root"),
            Self::ProjectDeleteRoot => f.write_str("project_delete_root"),
            Self::ProjectDelete => f.write_str("project_delete"),
            Self::WorkbenchCreate => f.write_str("workbench_create"),
            Self::WorkbenchRemove => f.write_str("workbench_remove"),
            Self::CustomAgentUpsert => f.write_str("custom_agent_upsert"),
            Self::CustomAgentDelete => f.write_str("custom_agent_delete"),
            Self::SteeringUpsert => f.write_str("steering_upsert"),
            Self::SteeringDelete => f.write_str("steering_delete"),
            Self::SkillRefresh => f.write_str("skill_refresh"),
            Self::McpServerUpsert => f.write_str("mcp_server_upsert"),
            Self::McpServerDelete => f.write_str("mcp_server_delete"),
            Self::TeamCreate => f.write_str("team_create"),
            Self::TeamRename => f.write_str("team_rename"),
            Self::TeamDelete => f.write_str("team_delete"),
            Self::TeamSetManager => f.write_str("team_set_manager"),
            Self::TeamMemberCreate => f.write_str("team_member_create"),
            Self::TeamMemberUpdate => f.write_str("team_member_update"),
            Self::TeamMemberDelete => f.write_str("team_member_delete"),
            Self::TeamMemberActivate => f.write_str("team_member_activate"),
            Self::TeamCompact => f.write_str("team_compact"),
            Self::TeamMemberShuffle => f.write_str("team_member_shuffle"),
            Self::TeamDraftCreate => f.write_str("team_draft_create"),
            Self::TeamDraftUpdate => f.write_str("team_draft_update"),
            Self::TeamDraftShuffle => f.write_str("team_draft_shuffle"),
            Self::TeamDraftApplyTemplate => f.write_str("team_draft_apply_template"),
            Self::TeamDraftCommit => f.write_str("team_draft_commit"),
            Self::TeamDraftDiscard => f.write_str("team_draft_discard"),
            Self::ProjectReadDiff => f.write_str("project_read_diff"),
            Self::ProjectReadFile => f.write_str("project_read_file"),
            Self::ProjectSearch => f.write_str("project_search"),
            Self::ProjectSearchCancel => f.write_str("project_search_cancel"),
            Self::ProjectAccessed => f.write_str("project_accessed"),
            Self::CodeIntelSubscribeFile => f.write_str("code_intel_subscribe_file"),
            Self::CodeIntelUnsubscribeFile => f.write_str("code_intel_unsubscribe_file"),
            Self::CodeIntelSetVisibleRange => f.write_str("code_intel_set_visible_range"),
            Self::CodeIntelHover => f.write_str("code_intel_hover"),
            Self::CodeIntelNavigate => f.write_str("code_intel_navigate"),
            Self::CodeIntelFindReferences => f.write_str("code_intel_find_references"),
            Self::CodeIntelCancelReferences => f.write_str("code_intel_cancel_references"),
            Self::ProjectStageFile => f.write_str("project_stage_file"),
            Self::ProjectStageHunk => f.write_str("project_stage_hunk"),
            Self::ProjectUnstageFile => f.write_str("project_unstage_file"),
            Self::ProjectDiscardFile => f.write_str("project_discard_file"),
            Self::ProjectGitCommit => f.write_str("project_git_commit"),
            Self::ProjectListDir => f.write_str("project_list_dir"),
            Self::HostBrowseStart => f.write_str("host_browse_start"),
            Self::HostBrowseList => f.write_str("host_browse_list"),
            Self::HostBrowseClose => f.write_str("host_browse_close"),
            Self::TerminalCreate => f.write_str("terminal_create"),
            Self::TerminalSend => f.write_str("terminal_send"),
            Self::TerminalResize => f.write_str("terminal_resize"),
            Self::TerminalClose => f.write_str("terminal_close"),
            Self::MobilePairingStart => f.write_str("mobile_pairing_start"),
            Self::MobilePairingCancel => f.write_str("mobile_pairing_cancel"),
            Self::MobileDeviceRevoke => f.write_str("mobile_device_revoke"),
            Self::MobileDeviceRename => f.write_str("mobile_device_rename"),
            Self::ClientError => f.write_str("client_error"),
            Self::Heartbeat => f.write_str("heartbeat"),
            Self::TriggerWorkflow => f.write_str("trigger_workflow"),
            Self::CancelWorkflow => f.write_str("cancel_workflow"),
            Self::WorkflowRefresh => f.write_str("workflow_refresh"),
            Self::HostBootstrap => f.write_str("host_bootstrap"),
            Self::AgentBootstrap => f.write_str("agent_bootstrap"),
            Self::ProjectBootstrap => f.write_str("project_bootstrap"),
            Self::ReviewBootstrap => f.write_str("review_bootstrap"),
            Self::BrowseBootstrap => f.write_str("browse_bootstrap"),
            Self::TerminalBootstrap => f.write_str("terminal_bootstrap"),
            Self::HostSettings => f.write_str("host_settings"),
            Self::AgentsViewPreferencesNotify => f.write_str("agents_view_preferences_notify"),
            Self::BackendSetup => f.write_str("backend_setup"),
            Self::NewAgent => f.write_str("new_agent"),
            Self::AgentActivitySummary => f.write_str("agent_activity_summary"),
            Self::AgentActivityStats => f.write_str("agent_activity_stats"),
            Self::TaskTokenUsage => f.write_str("task_token_usage"),
            Self::AgentStart => f.write_str("agent_start"),
            Self::AgentRenamed => f.write_str("agent_renamed"),
            Self::AgentCompactNotify => f.write_str("agent_compact_notify"),
            Self::AgentClosed => f.write_str("agent_closed"),
            Self::ChatEvent => f.write_str("chat_event"),
            Self::SessionHistory => f.write_str("session_history"),
            Self::AgentError => f.write_str("agent_error"),
            Self::QueuedMessages => f.write_str("queued_messages"),
            Self::SessionList => f.write_str("session_list"),
            Self::ProjectNotify => f.write_str("project_notify"),
            Self::CustomAgentNotify => f.write_str("custom_agent_notify"),
            Self::SteeringNotify => f.write_str("steering_notify"),
            Self::SkillNotify => f.write_str("skill_notify"),
            Self::McpServerNotify => f.write_str("mcp_server_notify"),
            Self::TeamNotify => f.write_str("team_notify"),
            Self::TeamMemberNotify => f.write_str("team_member_notify"),
            Self::TeamMemberBindingNotify => f.write_str("team_member_binding_notify"),
            Self::TeamCompactNotify => f.write_str("team_compact_notify"),
            Self::TeamPresetCatalogNotify => f.write_str("team_preset_catalog_notify"),
            Self::TeamDraftNotify => f.write_str("team_draft_notify"),
            Self::TeamMemberShuffleSuggestionNotify => {
                f.write_str("team_member_shuffle_suggestion_notify")
            }
            Self::ProjectFileList => f.write_str("project_file_list"),
            Self::ProjectGitStatus => f.write_str("project_git_status"),
            Self::ProjectFileContents => f.write_str("project_file_contents"),
            Self::ProjectSearchResults => f.write_str("project_search_results"),
            Self::ProjectSearchComplete => f.write_str("project_search_complete"),
            Self::CodeIntelOverview => f.write_str("code_intel_overview"),
            Self::CodeIntelStatus => f.write_str("code_intel_status"),
            Self::CodeIntelFileModel => f.write_str("code_intel_file_model"),
            Self::CodeIntelDiagnostics => f.write_str("code_intel_diagnostics"),
            Self::CodeIntelHoverResult => f.write_str("code_intel_hover_result"),
            Self::CodeIntelNavigateResult => f.write_str("code_intel_navigate_result"),
            Self::CodeIntelReferencesResults => f.write_str("code_intel_references_results"),
            Self::CodeIntelReferencesComplete => f.write_str("code_intel_references_complete"),
            Self::CodeIntelError => f.write_str("code_intel_error"),
            Self::ProjectGitDiff => f.write_str("project_git_diff"),
            Self::ProjectGitCommitResult => f.write_str("project_git_commit_result"),
            Self::NewTerminal => f.write_str("new_terminal"),
            Self::TerminalStart => f.write_str("terminal_start"),
            Self::TerminalOutput => f.write_str("terminal_output"),
            Self::TerminalExit => f.write_str("terminal_exit"),
            Self::TerminalError => f.write_str("terminal_error"),
            Self::HostBrowseOpened => f.write_str("host_browse_opened"),
            Self::HostBrowseEntries => f.write_str("host_browse_entries"),
            Self::HostBrowseError => f.write_str("host_browse_error"),
            Self::CommandError => f.write_str("command_error"),
            Self::SetSessionSettings => f.write_str("set_session_settings"),
            Self::SessionSchemas => f.write_str("session_schemas"),
            Self::SessionSettings => f.write_str("session_settings"),
            Self::BackendConfigSchemas => f.write_str("backend_config_schemas"),
            Self::BackendConfigSnapshots => f.write_str("backend_config_snapshots"),
            Self::BackendCapacity => f.write_str("backend_capacity"),
            Self::LaunchProfileCatalogNotify => f.write_str("launch_profile_catalog_notify"),
            Self::MobileAccessState => f.write_str("mobile_access_state"),
            Self::MobilePairingOffer => f.write_str("mobile_pairing_offer"),
            Self::ReviewCreate => f.write_str("review_create"),
            Self::ReviewAction => f.write_str("review_action"),
            Self::ReviewEvent => f.write_str("review_event"),
            Self::ReviewSubscribe => f.write_str("review_subscribe"),
            Self::ProjectEvent => f.write_str("project_event"),
            Self::WorkflowNotify => f.write_str("workflow_notify"),
            Self::WorkflowRunNotify => f.write_str("workflow_run_notify"),
            Self::HeartbeatAck => f.write_str("heartbeat_ack"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Envelope {
    pub stream: StreamPath,
    pub kind: FrameKind,
    pub seq: u64,
    pub payload: Value,
}

impl Envelope {
    pub fn from_payload<T: Serialize>(
        stream: StreamPath,
        kind: FrameKind,
        seq: u64,
        payload: &T,
    ) -> Result<Self, serde_json::Error> {
        Ok(Self {
            stream,
            kind,
            seq,
            payload: serde_json::to_value(payload)?,
        })
    }

    pub fn parse_payload<T: DeserializeOwned>(&self) -> Result<T, serde_json::Error> {
        serde_json::from_value(self.payload.clone())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HelloPayload {
    pub protocol_version: u32,
    pub tyde_version: Version,
    pub client_name: String,
    pub platform: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WelcomePayload {
    pub protocol_version: u32,
    pub tyde_version: Version,
    /// Exact, prerelease-capable host build version used by the web client to
    /// select the matching versioned bundle. `Option` for backward
    /// compatibility; `protocol_version`/`tyde_version` are unchanged so the
    /// exact-match handshake gate is unaffected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_version: Option<TydeReleaseVersion>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WorkflowCoordinatorSpec {
    pub backend: BackendKind,
    #[serde(default)]
    pub access_mode: BackendAccessMode,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowInputControl {
    #[default]
    Text,
    MultilineText,
    Boolean,
    Number,
    Select,
    FilePath,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WorkflowInputOption {
    pub value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WorkflowInputSpec {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub control: WorkflowInputControl,
    #[serde(default)]
    pub options: Vec<WorkflowInputOption>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TriggerSurface {
    GitPanel,
    ReviewHub,
    ChatInput,
    FileView { glob: String },
    Global,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkflowSourceScope {
    Global,
    Project {
        project_id: ProjectId,
        root: ProjectRootPath,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WorkflowSource {
    pub scope: WorkflowSourceScope,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WorkflowCatalogLocation {
    pub scope: WorkflowSourceScope,
    pub directory: String,
    pub exists: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkflowSaveTarget {
    Global,
    Project {
        project_id: ProjectId,
        root: ProjectRootPath,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum WorkflowSaveMode {
    Create,
    Replace {
        existing_path: String,
        existing_id: WorkflowId,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WorkflowTargetDirectory {
    pub target: WorkflowSaveTarget,
    pub location: WorkflowCatalogLocation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WorkflowTargetsResponse {
    pub targets: Vec<WorkflowTargetDirectory>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WorkflowSaveRequest {
    pub target: WorkflowSaveTarget,
    pub mode: WorkflowSaveMode,
    pub filename: String,
    pub markdown: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowDiagnosticSeverity {
    Error,
    Warning,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WorkflowDiagnostic {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_id: Option<WorkflowId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<WorkflowSource>,
    pub severity: WorkflowDiagnosticSeverity,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WorkflowSummary {
    pub id: WorkflowId,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub triggers: Vec<TriggerSurface>,
    #[serde(default)]
    pub inputs: Vec<WorkflowInputSpec>,
    pub coordinator: WorkflowCoordinatorSpec,
    #[serde(default)]
    pub declared_backends: Vec<BackendKind>,
    #[serde(default)]
    pub tags: Vec<String>,
    pub source: WorkflowSource,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WorkflowSaveResponse {
    pub summary: WorkflowSummary,
    pub source: WorkflowSource,
    pub path: String,
    pub created: bool,
    pub diagnostics: Vec<WorkflowDiagnostic>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowRunSnapshotStatus {
    Running,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowStepRunSnapshotStatus {
    Pending,
    Running,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentWorkflowMetadata {
    pub workflow_id: WorkflowId,
    pub workflow_run_id: WorkflowRunId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowStepRunSnapshot {
    pub id: WorkflowStepRunId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_step_id: Option<WorkflowStepRunId>,
    pub title: String,
    pub status: WorkflowStepRunSnapshotStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<AgentId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowRunSnapshot {
    pub id: WorkflowRunId,
    pub workflow_id: WorkflowId,
    pub workflow_name: String,
    pub source: WorkflowSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<ProjectId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coordinator_agent_id: Option<AgentId>,
    pub coordinator: WorkflowCoordinatorSpec,
    pub status: WorkflowRunSnapshotStatus,
    #[serde(default)]
    pub inputs: HashMap<String, Value>,
    #[serde(default)]
    pub steps: Vec<WorkflowStepRunSnapshot>,
    #[serde(default)]
    pub agent_ids: Vec<AgentId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowNotifyPayload {
    pub summaries: Vec<WorkflowSummary>,
    pub diagnostics: Vec<WorkflowDiagnostic>,
    #[serde(default)]
    pub locations: Vec<WorkflowCatalogLocation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowRunNotifyPayload {
    pub run: WorkflowRunSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TriggerWorkflowPayload {
    pub workflow_id: WorkflowId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<ProjectId>,
    #[serde(default)]
    pub inputs: HashMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CancelWorkflowPayload {
    pub run_id: WorkflowRunId,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowRefreshPayload {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostBootstrapPayload {
    pub settings: HostSettings,
    pub mobile_access: MobileAccessStatePayload,
    pub backend_setup: BackendSetupPayload,
    pub session_schemas: Vec<SessionSchemaEntry>,
    #[serde(default)]
    pub backend_config_schemas: Vec<BackendConfigSchema>,
    #[serde(default)]
    pub backend_config_snapshots: Vec<BackendConfigSnapshot>,
    #[serde(default)]
    pub launch_profile_catalog: LaunchProfileCatalog,
    pub sessions: Vec<SessionSummary>,
    pub session_list: SessionListPageInfo,
    pub projects: Vec<Project>,
    pub mcp_servers: Vec<McpServerConfig>,
    pub skills: Vec<Skill>,
    pub steering: Vec<Steering>,
    pub custom_agents: Vec<CustomAgent>,
    pub team_preset_catalog: TeamPresetCatalog,
    pub team_drafts: Vec<TeamDraft>,
    pub teams: Vec<Team>,
    pub team_members: Vec<TeamMember>,
    pub team_member_bindings: Vec<TeamMemberBindingPayload>,
    pub agents: Vec<NewAgentPayload>,
    #[serde(default)]
    pub task_token_usages: Vec<TaskTokenUsagePayload>,
    #[serde(default)]
    pub workflow_summaries: Vec<WorkflowSummary>,
    #[serde(default)]
    pub workflow_diagnostics: Vec<WorkflowDiagnostic>,
    #[serde(default)]
    pub workflow_runs: Vec<WorkflowRunSnapshot>,
    #[serde(default)]
    pub workflow_locations: Vec<WorkflowCatalogLocation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents_view_preferences: Option<AgentsViewPreferencesSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HostFilterId(pub String);

impl fmt::Display for HostFilterId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentsViewPreferences {
    pub filters: AgentsViewFilters,
    #[serde(default)]
    pub sort_mode: AgentSortMode,
    #[serde(default)]
    pub group_mode: AgentGroupMode,
    #[serde(default)]
    pub density: AgentListDensity,
    /// Deprecated: retained for protocol and persisted-store compatibility.
    /// Current clients no longer expose or apply hide-finished filtering.
    #[serde(default)]
    pub hide_finished: bool,
    #[serde(default)]
    pub manual_order: Vec<AgentOrderKey>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentsViewFilters {
    #[serde(default)]
    pub host_ids: Vec<HostFilterId>,
    #[serde(default)]
    pub project_ids: Vec<AgentProjectFilter>,
    #[serde(default)]
    pub statuses: Vec<AgentStatusFilter>,
    #[serde(default)]
    pub backends: Vec<BackendKind>,
    #[serde(default)]
    pub origins: Vec<AgentOrigin>,
    #[serde(default)]
    pub tags: Vec<AgentTagRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AgentProjectFilter {
    pub host_id: HostFilterId,
    pub project_id: ProjectId,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentSortMode {
    #[default]
    ManualThenActivity,
    NewestFirst,
    OldestFirst,
    NameAsc,
    Status,
    Backend,
    Project,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentGroupMode {
    #[default]
    Flat,
    Status,
    Backend,
    Project,
    /// Group by tag. Agents with multiple tags may be rendered under each tag
    /// group by clients; untagged agents belong in an explicit untagged group.
    Tag,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentListDensity {
    #[default]
    Comfortable,
    Compact,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatusFilter {
    Initializing,
    Thinking,
    Compacting,
    Idle,
    Terminated,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentOrderKey {
    Session {
        session_id: SessionId,
    },
    TransientAgent {
        host_id: HostFilterId,
        agent_id: AgentId,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentManualTagId(pub String);

impl fmt::Display for AgentManualTagId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentSystemTagId(pub String);

impl fmt::Display for AgentSystemTagId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "tag_id", rename_all = "snake_case")]
pub enum AgentTagRef {
    Manual(AgentManualTagId),
    System(AgentSystemTagId),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentTagColor(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentAnnotationTarget {
    Session {
        host_id: HostFilterId,
        session_id: SessionId,
    },
    TransientAgent {
        host_id: HostFilterId,
        agent_id: AgentId,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentManualTagDescriptor {
    pub id: AgentManualTagId,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<AgentTagColor>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSystemTagDescriptor {
    pub id: AgentSystemTagId,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<AgentTagColor>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentManualTagAssignment {
    pub target: AgentAnnotationTarget,
    pub tag_ids: Vec<AgentManualTagId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSystemTagAssignment {
    pub target: AgentAnnotationTarget,
    pub tag_ids: Vec<AgentSystemTagId>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentTagsSnapshot {
    #[serde(default)]
    pub manual: Vec<AgentManualTagDescriptor>,
    #[serde(default)]
    pub system: Vec<AgentSystemTagDescriptor>,
    #[serde(default)]
    pub manual_assignments: Vec<AgentManualTagAssignment>,
    #[serde(default)]
    pub system_assignments: Vec<AgentSystemTagAssignment>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentPinsSnapshot {
    /// Pinned agents are an outer section hint for clients. They do not bypass
    /// active filters or Smart Views; filtered-out pinned agents stay hidden.
    #[serde(default)]
    pub pinned: Vec<AgentAnnotationTarget>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentGroupId(pub String);

impl fmt::Display for AgentGroupId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentGroup {
    pub id: AgentGroupId,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentGroupAssignment {
    pub group_id: AgentGroupId,
    pub target: AgentAnnotationTarget,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentGroupsSnapshot {
    #[serde(default)]
    pub groups: Vec<AgentGroup>,
    #[serde(default)]
    pub assignments: Vec<AgentGroupAssignment>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentsSidebarProjectVisibility {
    #[default]
    ContextualDefault,
    CurrentProjectOnly,
    AllProjects,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentsSidebarPreferences {
    #[serde(default)]
    pub hide_inactive: bool,
    #[serde(default)]
    pub hide_sub_agents: bool,
    #[serde(default)]
    pub project_visibility: AgentsSidebarProjectVisibility,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentTagsUpdate {
    CreateTag {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        color: Option<AgentTagColor>,
    },
    RenameTag {
        tag_id: AgentManualTagId,
        name: String,
    },
    SetTagColor {
        tag_id: AgentManualTagId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        color: Option<AgentTagColor>,
    },
    DeleteTag {
        tag_id: AgentManualTagId,
    },
    AssignTag {
        target: AgentAnnotationTarget,
        tag_id: AgentManualTagId,
    },
    RemoveTag {
        target: AgentAnnotationTarget,
        tag_id: AgentManualTagId,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetAgentTagsPayload {
    pub update: AgentTagsUpdate,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentPinsUpdate {
    Pin { target: AgentAnnotationTarget },
    Unpin { target: AgentAnnotationTarget },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetAgentPinsPayload {
    pub update: AgentPinsUpdate,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentGroupsUpdate {
    CreateGroup {
        name: String,
        targets: Vec<AgentAnnotationTarget>,
    },
    RenameGroup {
        id: AgentGroupId,
        name: String,
    },
    DeleteGroup {
        id: AgentGroupId,
    },
    MoveTargets {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        group_id: Option<AgentGroupId>,
        targets: Vec<AgentAnnotationTarget>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetAgentGroupsPayload {
    pub update: AgentGroupsUpdate,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentsViewPreferencesUpdate {
    SetFilters {
        filters: AgentsViewFilters,
    },
    SetSortMode {
        sort_mode: AgentSortMode,
    },
    SetGroupMode {
        group_mode: AgentGroupMode,
    },
    SetDensity {
        density: AgentListDensity,
    },
    /// Deprecated: retained so older clients can deserialize/round-trip the
    /// preference during the protocol-20 compatibility window.
    SetHideFinished {
        hide_finished: bool,
    },
    SetManualOrder {
        manual_order: Vec<AgentOrderKey>,
    },
    SetSidebarPreferences {
        sidebar: AgentsSidebarPreferences,
    },
    Reset,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetAgentsViewPreferencesPayload {
    pub update: AgentsViewPreferencesUpdate,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SmartView {
    pub id: SmartViewId,
    pub name: String,
    pub filters: AgentsViewFilters,
    #[serde(default)]
    pub sort_mode: AgentSortMode,
    #[serde(default)]
    pub group_mode: AgentGroupMode,
    /// Deprecated: retained for protocol and persisted Smart View
    /// compatibility. Current clients ignore this field.
    #[serde(default)]
    pub hide_finished: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum SmartViewId {
    BuiltIn(BuiltInSmartViewId),
    User(UserSmartViewId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuiltInSmartViewId {
    All,
    Active,
    FailedTerminated,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UserSmartViewId(pub String);

impl fmt::Display for UserSmartViewId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentsSmartViewsSnapshot {
    #[serde(default)]
    pub built_in: Vec<SmartView>,
    #[serde(default)]
    pub user: Vec<SmartView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_view_id: Option<SmartViewId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentsSmartViewsUpdate {
    SaveCurrent { name: String },
    Rename { id: SmartViewId, name: String },
    Update { id: SmartViewId },
    Delete { id: SmartViewId },
    Reorder { user_ids: Vec<SmartViewId> },
    SetActive { id: SmartViewId },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetAgentsSmartViewsPayload {
    pub update: AgentsSmartViewsUpdate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentsViewPreferencesStoreErrorKind {
    Corrupt,
    UnsupportedVersion,
    Io,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentsViewPreferencesStoreError {
    pub kind: AgentsViewPreferencesStoreErrorKind,
    pub message: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentsViewPreferencesSnapshot {
    pub preferences: AgentsViewPreferences,
    #[serde(default)]
    pub sidebar: AgentsSidebarPreferences,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load_error: Option<AgentsViewPreferencesStoreError>,
    #[serde(default)]
    pub smart_views: AgentsSmartViewsSnapshot,
    #[serde(default)]
    pub tags: AgentTagsSnapshot,
    #[serde(default)]
    pub pins: AgentPinsSnapshot,
    #[serde(default)]
    pub groups: AgentGroupsSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentsViewPreferencesNotifyPayload {
    pub snapshot: AgentsViewPreferencesSnapshot,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentBootstrapPayload {
    pub events: Vec<AgentBootstrapEvent>,
    pub latest_output: AgentControlOutput,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "payload", rename_all = "snake_case")]
pub enum AgentBootstrapEvent {
    AgentStart(AgentStartPayload),
    AgentError(AgentErrorPayload),
    SessionSettings(SessionSettingsPayload),
    QueuedMessages(QueuedMessagesPayload),
    AgentActivityStats(AgentActivityStatsPayload),
    ChatEvent(ChatEvent),
    HasPriorHistory { message_count: u32, before_seq: u64 },
}

impl AgentBootstrapEvent {
    pub fn frame_kind(&self) -> FrameKind {
        match self {
            Self::AgentStart(_) => FrameKind::AgentStart,
            Self::AgentError(_) => FrameKind::AgentError,
            Self::SessionSettings(_) => FrameKind::SessionSettings,
            Self::QueuedMessages(_) => FrameKind::QueuedMessages,
            Self::AgentActivityStats(_) => FrameKind::AgentActivityStats,
            Self::ChatEvent(_) => FrameKind::ChatEvent,
            Self::HasPriorHistory { .. } => FrameKind::AgentBootstrap,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectBootstrapPayload {
    pub project: Project,
    pub file_list: ProjectFileListPayload,
    pub git_status: ProjectGitStatusPayload,
    pub review_summaries: Vec<ReviewSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewBootstrapPayload {
    pub review: Review,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowseBootstrapPayload {
    pub opened: HostBrowseOpenedPayload,
    pub listing: BrowseBootstrapListing,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BrowseBootstrapListing {
    Entries { entries: HostBrowseEntriesPayload },
    Error { error: HostBrowseErrorPayload },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalBootstrapPayload {
    pub terminal_id: TerminalId,
    pub start: TerminalStartPayload,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    pub tyde_debug_mcp_enabled: bool,
    #[serde(default = "default_agent_control_mcp_enabled")]
    pub tyde_agent_control_mcp_enabled: bool,
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
    /// [`BackendConfigSchema`]. Backends without an entry use their defaults.
    #[serde(default)]
    pub backend_config: HashMap<BackendKind, BackendConfigValues>,
    /// Explicit server-owned Launch Profiles. These are host-level presets
    /// over backend session settings; they are never inferred from model names.
    #[serde(default)]
    pub launch_profiles: Vec<HostLaunchProfileConfig>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeIntelSettings {
    #[serde(default)]
    pub language_server_paths: HashMap<CodeIntelProviderId, HostExecutablePath>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HostExecutablePath(pub String);

impl fmt::Display for HostExecutablePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupervisorSettings {
    #[serde(default)]
    pub enabled: bool,
    /// When the supervisor judges the task complete, automatically compact
    /// (rotate-and-summarize) the agent so reusing it later starts from a
    /// small warm context instead of resuming a huge cold session.
    #[serde(default)]
    pub auto_compact_on_success: bool,
    /// Maximum consecutive supervisor kicks without an intervening real user
    /// message. Prevents a supervisor/agent ping-pong loop.
    #[serde(default = "default_supervisor_max_kicks_per_task")]
    pub max_kicks_per_task: u8,
    /// Extra attempts when a supervision call errors or returns output that
    /// does not parse to a verdict. 1 means one retry after the first failure.
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
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
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

impl Default for SupervisorSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            auto_compact_on_success: false,
            max_kicks_per_task: default_supervisor_max_kicks_per_task(),
            retry_attempts: default_supervisor_retry_attempts(),
            cost_tier: SupervisorCostTier::default(),
        }
    }
}

/// Prefix every supervisor-authored kick message carries. It keeps supervisor
/// turns visibly labeled in the transcript and lets the server count
/// consecutive supervisor kicks straight from the event log, with no
/// per-agent bookkeeping that could survive or miss restarts.
pub const SUPERVISOR_MESSAGE_PREFIX: &str = "[Tyde Supervisor] ";

/// Per-backend mapping from spawn complexity tiers to session-settings
/// overrides (e.g. `model`, `effort`). An empty map means "no override" —
/// the spawn runs on the backend's own defaults.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendTierConfig {
    #[serde(default)]
    pub low: SessionSettingsValues,
    #[serde(default)]
    pub high: SessionSettingsValues,
}

fn default_agent_control_mcp_enabled() -> bool {
    true
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetSettingPayload {
    pub setting: HostSettingValue,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HostSettingValue {
    EnabledBackends {
        enabled_backends: Vec<BackendKind>,
    },
    DefaultBackend {
        default_backend: Option<BackendKind>,
    },
    EnableMobileConnections {
        enabled: bool,
    },
    MobileBrokerUrl {
        broker_url: Option<BrokerUrl>,
    },
    TydeDebugMcpEnabled {
        enabled: bool,
    },
    TydeAgentControlMcpEnabled {
        enabled: bool,
    },
    ComplexityTiersEnabled {
        enabled: bool,
    },
    BackendTiers {
        backend: BackendKind,
        config: BackendTierConfig,
    },
    BackgroundAgentFeatureEnabled {
        feature: BackgroundAgentFeature,
        enabled: bool,
    },
    SupervisorEnabled {
        enabled: bool,
    },
    SupervisorAutoCompactOnSuccess {
        enabled: bool,
    },
    SupervisorMaxKicksPerTask {
        count: u8,
    },
    SupervisorRetryAttempts {
        count: u8,
    },
    SupervisorCostTier {
        tier: SupervisorCostTier,
    },
    CodeIntelLanguageServerPath {
        provider: CodeIntelProviderId,
        path: Option<HostExecutablePath>,
    },
    /// Merge a deep-configuration update for one backend. Keys present with
    /// non-null values are validated against the backend's schema and saved.
    /// Keys present as `Null` are explicitly cleared. Missing keys are
    /// preserved, so editing one field cannot overwrite sibling config. An
    /// empty `values` map explicitly clears the backend's whole configuration.
    BackendConfig {
        backend: BackendKind,
        values: BackendConfigValues,
    },
    /// Replace a backend-native settings document through the backend's own
    /// settings protocol. Tyde does not persist this payload in host settings.
    BackendNativeSettings {
        backend: BackendKind,
        settings: Value,
    },
    /// Acknowledge the one-time notice for a specific server-owned Tycode
    /// managed-settings projection. A mismatched projection id is a typed
    /// `CommandErrorCode::Conflict`, never an acknowledgement of a newer
    /// projection.
    AcknowledgeTycodeProjectionNotice {
        backend: BackendKind,
        projection_id: TycodeProjectionId,
    },
    /// Clear only the server-owned Tycode managed projection after an explicit
    /// recovery-required state. Both tokens must exactly match the state the
    /// server reported; a stale token is a typed `CommandErrorCode::Conflict`.
    ResetTycodeManagedProjection {
        backend: BackendKind,
        expected_projection_id: TycodeProjectionId,
        expected_state_hash: TycodeProjectionStateHash,
    },
    /// Replace all explicit server-owned Launch Profiles.
    LaunchProfiles {
        profiles: Vec<HostLaunchProfileConfig>,
    },
}

impl HostSettingValue {
    /// Returns the value-free target clients use to correlate a failed setting
    /// command with its pending save.
    pub fn error_target(&self) -> HostSettingErrorTarget {
        match self {
            Self::EnabledBackends { .. } => HostSettingErrorTarget::EnabledBackends,
            Self::DefaultBackend { .. } => HostSettingErrorTarget::DefaultBackend,
            Self::EnableMobileConnections { .. } => HostSettingErrorTarget::EnableMobileConnections,
            Self::MobileBrokerUrl { .. } => HostSettingErrorTarget::MobileBrokerUrl,
            Self::TydeDebugMcpEnabled { .. } => HostSettingErrorTarget::TydeDebugMcpEnabled,
            Self::TydeAgentControlMcpEnabled { .. } => {
                HostSettingErrorTarget::TydeAgentControlMcpEnabled
            }
            Self::ComplexityTiersEnabled { .. } => HostSettingErrorTarget::ComplexityTiersEnabled,
            Self::BackendTiers { .. } => HostSettingErrorTarget::BackendTiers,
            Self::BackgroundAgentFeatureEnabled { .. } => {
                HostSettingErrorTarget::BackgroundAgentFeatureEnabled
            }
            Self::SupervisorEnabled { .. } => HostSettingErrorTarget::SupervisorEnabled,
            Self::SupervisorAutoCompactOnSuccess { .. } => {
                HostSettingErrorTarget::SupervisorAutoCompactOnSuccess
            }
            Self::SupervisorMaxKicksPerTask { .. } => {
                HostSettingErrorTarget::SupervisorMaxKicksPerTask
            }
            Self::SupervisorRetryAttempts { .. } => HostSettingErrorTarget::SupervisorRetryAttempts,
            Self::SupervisorCostTier { .. } => HostSettingErrorTarget::SupervisorCostTier,
            Self::CodeIntelLanguageServerPath { .. } => {
                HostSettingErrorTarget::CodeIntelLanguageServerPath
            }
            Self::BackendConfig { .. } => HostSettingErrorTarget::BackendConfig,
            Self::BackendNativeSettings { .. } => HostSettingErrorTarget::BackendNativeSettings,
            Self::AcknowledgeTycodeProjectionNotice { .. } => {
                HostSettingErrorTarget::AcknowledgeTycodeProjectionNotice
            }
            Self::ResetTycodeManagedProjection { .. } => {
                HostSettingErrorTarget::ResetTycodeManagedProjection
            }
            Self::LaunchProfiles { .. } => HostSettingErrorTarget::LaunchProfiles,
        }
    }
}

/// Identifies the setting affected by a [`FrameKind::SetSetting`] command
/// error without echoing a submitted value, credential, path, or stale token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostSettingErrorTarget {
    /// The command payload was not a valid typed host-setting value.
    Malformed,
    EnabledBackends,
    DefaultBackend,
    EnableMobileConnections,
    MobileBrokerUrl,
    TydeDebugMcpEnabled,
    TydeAgentControlMcpEnabled,
    ComplexityTiersEnabled,
    BackendTiers,
    BackgroundAgentFeatureEnabled,
    SupervisorEnabled,
    SupervisorAutoCompactOnSuccess,
    SupervisorMaxKicksPerTask,
    SupervisorRetryAttempts,
    SupervisorCostTier,
    CodeIntelLanguageServerPath,
    BackendConfig,
    BackendNativeSettings,
    AcknowledgeTycodeProjectionNotice,
    ResetTycodeManagedProjection,
    LaunchProfiles,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostSettingsPayload {
    pub settings: HostSettings,
}

/// Deep, host-level configuration schema for one backend. Rendered in the
/// settings panel (not the per-session settings bar). The frontend
/// auto-generates form controls from `fields`, exactly like
/// [`SessionSettingsSchema`], but with a richer field-type set (free text,
/// secrets) suited to setup/configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendConfigSchema {
    pub backend_kind: BackendKind,
    pub persistence_mode: BackendConfigPersistenceMode,
    pub fields: Vec<BackendConfigField>,
}

/// Where persisted backend configuration is written. This lets clients render
/// backend-owned setup state without hardcoding backend names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendConfigPersistenceMode {
    /// Values are stored in Tyde host settings and applied when spawning.
    TydeSettingsStore,
    /// Values are written to the backend-native configuration source and
    /// require that backend to be installed/runnable on the host.
    BackendNative,
}

/// One configurable field in a backend's deep configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendConfigField {
    /// Machine-readable key, e.g. "default_model".
    pub key: String,
    /// Human-readable label for the UI.
    pub label: String,
    /// Optional description shown as help text.
    pub description: Option<String>,
    /// The type and constraints of this field.
    pub field_type: BackendConfigFieldType,
}

/// The type of a backend-config field. Determines how the frontend renders it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BackendConfigFieldType {
    /// Free-text single- or multi-line input.
    Text {
        #[serde(default)]
        default: Option<String>,
        #[serde(default)]
        placeholder: Option<String>,
        #[serde(default)]
        multiline: bool,
    },
    /// Masked secret input. Never pre-filled with the stored value on render.
    Secret {
        #[serde(default)]
        placeholder: Option<String>,
    },
    Select {
        options: Vec<SelectOption>,
        default: Option<String>,
        nullable: bool,
    },
    Toggle {
        default: bool,
    },
    Integer {
        min: i64,
        max: i64,
        step: i64,
        default: i64,
    },
}

/// Current deep-configuration values for one backend.
/// Keys match `BackendConfigField.key`. Values reuse the session-setting value
/// enum (`String`/`Bool`/`Integer`/`Null`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendConfigValues(pub HashMap<String, SessionSettingValue>);

/// Server → Client on host stream. Carries the host/build's deep-config schema
/// catalog for every backend that exposes one. Enabled-backend state does not
/// filter this catalog; backends without deep config are omitted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendConfigSchemasPayload {
    pub schemas: Vec<BackendConfigSchema>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendConfigSnapshotStatus {
    Ready,
    Unavailable,
}

/// Server-owned snapshot of a backend's current native configuration. These
/// values are read from the backend-native source of truth and are not a
/// replacement for `HostSettings.backend_config`, which stores only explicit
/// Tyde-managed overrides.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendConfigSnapshot {
    pub backend_kind: BackendKind,
    pub status: BackendConfigSnapshotStatus,
    #[serde(default)]
    pub values: BackendConfigValues,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Server → Client on host stream. Carries current backend-native settings
/// snapshots for enabled backends that expose deep configuration. Snapshot
/// probing remains runtime-driven and separate from the schema catalog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendConfigSnapshotsPayload {
    pub snapshots: Vec<BackendConfigSnapshot>,
    /// Backend-native, JSON-schema-driven settings snapshots. These carry the
    /// backend's current settings document and grouped schemas as one typed
    /// server-owned state update for UIs that render backend-native settings.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub native_settings: Vec<BackendNativeSettingsSnapshot>,
}

/// Server-owned, host-scoped subscription-capacity state. Capacity is advisory
/// data reported by a backend; it is never an input to agent routing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendCapacityPayload {
    pub snapshots: Vec<BackendCapacitySnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendCapacitySnapshot {
    pub backend_kind: BackendKind,
    pub state: BackendCapacityState,
    /// Host time when the server received the current report or state.
    pub retrieved_at_ms: u64,
    pub freshness: CapacityFreshness,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BackendCapacityState {
    Known {
        report: CapacityReport,
    },
    Stale {
        report: CapacityReport,
        stale_since_ms: u64,
    },
    Unavailable {
        reason: CapacityUnavailableReason,
    },
    Unsupported {
        reason: CapacityUnsupportedReason,
    },
    AuthError {
        detail: CapacityErrorDetail,
    },
    RateLimited {
        detail: CapacityErrorDetail,
        retry_at_ms: Option<u64>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapacityUnavailableReason {
    AwaitingFirstReport,
    MalformedReport,
    SourceUnreachable,
    SourceTimedOut,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapacityUnsupportedReason {
    BackendHasNoCapacitySource,
    BackendVersionTooOld,
    AccountTypeNotReported,
    ExternalProvider,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapacityErrorDetail {
    pub summary: String,
    pub code: CapacityErrorCode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapacityErrorCode {
    NotAuthenticated,
    SourceRejected,
    RateLimited,
    MalformedResponse,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CapacityFreshness {
    Fresh { age_ms: u64 },
    Stale { age_ms: u64, threshold_ms: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapacityReport {
    pub source: CapacitySource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<CapacityPlanLabel>,
    pub buckets: Vec<CapacityBucket>,
    pub coverage: CapacityCoverage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapacitySource {
    CodexAccountRateLimitsUpdated,
    ClaudeRateLimitEvent,
    ClaudeControlUsage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapacityCoverage {
    AllVendorBuckets,
    RepresentativeBucketOnly,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapacityPlanLabel {
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapacityBucket {
    pub id: CapacityBucketId,
    pub label: String,
    pub measure: CapacityMeasure,
    pub scope: CapacityScope,
    pub window: CapacityWindow,
    pub reset: CapacityReset,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<CapacityBucketStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "vendor", rename_all = "snake_case")]
pub enum CapacityBucketId {
    Codex { slot: CodexLimitSlot },
    Claude { limit: ClaudeLimitType },
    ClaudeModel { name: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodexLimitSlot {
    Primary,
    Secondary,
    Credits,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaudeLimitType {
    FiveHour,
    SevenDay,
    SevenDayOpus,
    SevenDaySonnet,
    SevenDayOverageIncluded,
    Overage,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CapacityMeasure {
    UsedPercent {
        used_percent: u8,
        remaining_percent: u8,
        provenance: ValueProvenance,
    },
    Credits {
        has_credits: bool,
        unlimited: bool,
        balance: Option<String>,
    },
    ReportedWithoutMagnitude,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValueProvenance {
    pub vendor_reported: bool,
}

/// Provenance is per displayed value, not per bucket. `used_percent` comes
/// directly from the passive vendor notification; `remaining_percent` is its
/// safe complement. `ValueProvenance` remains the wire-compatible description
/// of the used value for existing clients.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PercentValueProvenance {
    VendorReported,
    DerivedFromVendorTotals,
    DerivedComplement,
}

impl CapacityMeasure {
    pub fn used_percent_provenance(&self) -> Option<PercentValueProvenance> {
        match self {
            Self::UsedPercent { provenance, .. } if provenance.vendor_reported => {
                Some(PercentValueProvenance::VendorReported)
            }
            Self::UsedPercent { .. } => Some(PercentValueProvenance::DerivedFromVendorTotals),
            _ => None,
        }
    }

    pub fn remaining_percent_provenance(&self) -> Option<PercentValueProvenance> {
        matches!(self, Self::UsedPercent { .. })
            .then_some(PercentValueProvenance::DerivedComplement)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CapacityScope {
    Account,
    Workspace,
    Individual,
    ModelFamily { name: String },
    OrganizationSpend,
    NotReported,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CapacityWindow {
    Rolling { duration_minutes: u32 },
    NotReported,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CapacityReset {
    At { at_ms: u64 },
    NotReported,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapacityBucketStatus {
    Allowed,
    AllowedWarning,
    Rejected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendNativeSettingsGroupKind {
    Core,
    Module,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendNativeSettingsGroup {
    pub id: String,
    pub title: String,
    pub kind: BackendNativeSettingsGroupKind,
    /// Path inside the backend settings object whose value this group edits.
    /// Empty means the group's schema properties are top-level settings fields.
    #[serde(default)]
    pub settings_path: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub schema: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendNativeSettingsSnapshot {
    pub backend_kind: BackendKind,
    pub status: BackendConfigSnapshotStatus,
    /// Current backend-native settings values. Omitted when unavailable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings: Option<Value>,
    /// Grouped JSON schemas that describe editable regions of `settings`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<BackendNativeSettingsGroup>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Ownership/provenance for a server-managed native-settings projection.
    /// Omitted for backends and snapshots that do not use a managed projection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<BackendNativeSettingsProvenance>,
    /// Non-fatal diagnostics from a ready backend-native settings operation.
    /// They remain typed so renderers never infer settings safety from text.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub advisories: Vec<BackendNativeSettingsAdvisory>,
    /// Server-owned recovery state for a managed native-settings projection.
    /// It is absent during normal operation and never inferred by clients.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub managed_projection_recovery: Option<TycodeManagedProjectionRecoveryState>,
}

/// Server-owned provenance for a backend-native settings snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BackendNativeSettingsProvenance {
    TycodeManagedProjection {
        managed_settings_path: HostAbsPath,
        source_settings_path: HostAbsPath,
        source: TycodeProjectionSource,
        tycode_version: Version,
        projection_id: TycodeProjectionId,
        created_at_ms: u64,
        source_digest: TycodeProjectionSourceDigest,
        original_unchanged: bool,
        notice_pending: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TycodeProjectionSource {
    SharedSettings,
    Defaults,
}

/// Stable identity for one managed Tycode settings projection. The server
/// compares this exact value before acknowledging a one-time notice.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TycodeProjectionId(pub String);

/// Digest of the original source bytes used when the managed projection was
/// created. It is opaque to clients; only the server establishes its value.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TycodeProjectionSourceDigest(pub String);

/// Opaque hash of the server-observed managed projection and its transaction
/// artifacts. It is compared exactly before an explicit managed reset.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TycodeProjectionStateHash(pub String);

/// Explicit recovery state for a managed Tycode projection. The UI may offer a
/// reset only for this server-emitted state; it never reconstructs recovery
/// needs from paths, files, or message text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TycodeManagedProjectionRecoveryState {
    ManagedProjectionResetRequired {
        reason: String,
        expected_projection_id: TycodeProjectionId,
        expected_state_hash: TycodeProjectionStateHash,
    },
}

/// Non-fatal, server-classified advisory associated with a native settings
/// snapshot. A `Ready` snapshot may carry one or more advisories.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BackendNativeSettingsAdvisory {
    NoProviderConfigured { message: String },
    UnsupportedActiveProvider { provider: String, message: String },
    BackendReported { message: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientErrorPayload {
    pub code: ClientErrorCode,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_context: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeartbeatPayload {
    pub client_sent_at_ms: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MobilePairingStartPayload {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MobilePairingCancelPayload {
    pub offer_id: MobilePairingOfferId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MobileDeviceRevokePayload {
    pub device_id: MobileDeviceId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MobileDeviceRenamePayload {
    pub device_id: MobileDeviceId,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MobileAccessStatePayload {
    pub broker_status: MobileBrokerStatus,
    pub pairing: MobilePairingState,
    pub paired_devices: Vec<MobileDeviceSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MobilePairingOfferPayload {
    pub offer_id: MobilePairingOfferId,
    pub qr_uri: MobilePairingQrUri,
    pub expires_at_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagedBrokerProvider {
    AwsIotCore,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagedBrokerRole {
    Host,
    Mobile,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedBrokerEndpoint {
    pub endpoint: BrokerUrl,
    pub provider: ManagedBrokerProvider,
    pub region: ManagedBrokerRegion,
    pub authorizer_name: ManagedBrokerAuthorizerName,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedBrokerConnectAuth {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub websocket_url: Option<BrokerUrl>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
}

impl fmt::Debug for ManagedBrokerConnectAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ManagedBrokerConnectAuth")
            .field("username", &self.username.as_ref().map(|_| "<redacted>"))
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field(
                "websocket_url",
                &self.websocket_url.as_ref().map(|_| "<redacted>"),
            )
            .field("header_count", &self.headers.len())
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedBrokerCredentialScope {
    pub namespace: ManagedBrokerTopicNamespace,
    pub role: ManagedBrokerRole,
    pub publish: Vec<String>,
    pub subscribe: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedBrokerCredentials {
    pub grant_id: ManagedBrokerGrantId,
    pub client_id: ManagedBrokerClientId,
    pub connect: ManagedBrokerConnectAuth,
    pub scope: ManagedBrokerCredentialScope,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MobileBrokerStatus {
    Disabled,
    Connecting {
        broker_url: BrokerUrl,
    },
    Online {
        broker_url: BrokerUrl,
    },
    Error {
        broker_url: Option<BrokerUrl>,
        code: MobileAccessErrorCode,
        message: String,
    },
    RepairRequired {
        code: MobileAccessErrorCode,
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MobilePairingState {
    Idle,
    Active {
        offer_id: MobilePairingOfferId,
        expires_at_ms: u64,
    },
    Consumed {
        offer_id: MobilePairingOfferId,
    },
    Expired {
        offer_id: MobilePairingOfferId,
    },
    Cancelled {
        offer_id: MobilePairingOfferId,
    },
    Failed {
        offer_id: MobilePairingOfferId,
        code: MobileAccessErrorCode,
        message: String,
    },
    RepairRequired {
        code: MobileAccessErrorCode,
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MobileServiceAuthStatePayload {
    pub state: MobileServiceAuthState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MobileServiceAuthState {
    Idle,
    Authenticating,
    Authenticated {
        expires_at_ms: u64,
    },
    PassRequired {
        message: String,
        paywall_url: String,
    },
    AuthFailed {
        message: String,
    },
    ServiceUnavailable {
        message: String,
        retryable: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MobileDeviceState {
    Paired,
    Connected,
    Revoked,
    RepairRequired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MobileAccessErrorCode {
    InvalidConfig,
    PassRequired,
    RepairRequired,
    ServiceAuthRequired,
    ServiceAuthFailed,
    ServiceUnavailable,
    BrokerUnavailable,
    BrokerConnectionFailed,
    BrokerProtocol,
    BrokerRejected,
    PairingExpired,
    PairingRejected,
    CryptoFailed,
    DuplicateDevice,
    InvalidPairingQr,
    KeystoreFailed,
    StoreLoadFailed,
    TransportFailed,
    UnknownDevice,
    RevokedDevice,
    VersionMismatch,
    Internal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MobileDeviceSummary {
    pub device_id: MobileDeviceId,
    pub label: String,
    pub key_fingerprint: String,
    pub created_at_ms: u64,
    pub last_seen_at_ms: Option<u64>,
    pub state: MobileDeviceState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendSetupStatus {
    Installed,
    NotInstalled,
    Unavailable,
    Unsupported,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendSetupDiagnosticCode {
    CommandNotFound,
    CommandFailed,
    CommandTimedOut,
    MissingProjectRoot,
    MissingGatewayPython,
    GatewayImportFailed,
    ExplicitOverrideInvalid,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendSetupDiagnostic {
    pub code: BackendSetupDiagnosticCode,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendSetupAction {
    Install,
    SignIn,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendSetupCommand {
    pub title: String,
    pub description: String,
    pub command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_command: Option<String>,
    pub runnable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendSetupInfo {
    pub backend_kind: BackendKind,
    pub status: BackendSetupStatus,
    pub installed_version: Option<String>,
    pub docs_url: String,
    pub install_command: Option<BackendSetupCommand>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostic: Option<BackendSetupDiagnostic>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sign_in_command: Option<BackendSetupCommand>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendSetupPayload {
    pub backends: Vec<BackendSetupInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunBackendSetupPayload {
    pub backend_kind: BackendKind,
    pub action: BackendSetupAction,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RejectPayload {
    pub code: RejectCode,
    pub message: String,
    pub server_protocol_version: u32,
    pub server_tyde_version: Version,
    /// Exact, prerelease-capable host build version (see [`WelcomePayload`]),
    /// so a rejected web client can self-heal by booting the host's bundle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_version: Option<TydeReleaseVersion>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectCode {
    IncompatibleProtocol,
    InvalidHandshake,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpawnAgentPayload {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_agent_id: Option<CustomAgentId>,
    pub parent_agent_id: Option<AgentId>,
    pub project_id: Option<ProjectId>,
    pub params: SpawnAgentParams,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SpawnAgentParams {
    New {
        workspace_roots: Vec<String>,
        prompt: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        images: Option<Vec<ImageData>>,
        backend_kind: BackendKind,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        launch_profile_id: Option<LaunchProfileId>,
        cost_hint: Option<SpawnCostHint>,
        #[serde(default)]
        access_mode: BackendAccessMode,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_settings: Option<SessionSettingsValues>,
    },
    Resume {
        session_id: SessionId,
        prompt: Option<String>,
    },
    Fork {
        from_session_id: SessionId,
        prompt: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        images: Option<Vec<ImageData>>,
        // Deserializing a missing field applies the side-question default
        // (`read_only`), while serializing omits only an explicit `None`.
        #[serde(
            default = "default_fork_access_mode",
            skip_serializing_if = "Option::is_none"
        )]
        access_mode: Option<BackendAccessMode>,
    },
}

fn default_fork_access_mode() -> Option<BackendAccessMode> {
    Some(BackendAccessMode::ReadOnly)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendMessagePayload {
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub images: Option<Vec<ImageData>>,
    #[serde(default)]
    pub origin: Option<MessageOrigin>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_response: Option<SendMessageToolResponse>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MessageOrigin {
    User,
    Review {
        review_id: ReviewId,
    },
    /// Sent by the hidden agent supervisor to kick a stalled agent back to
    /// work after it went idle without finishing its task.
    Supervisor,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum SendMessageToolResponse {
    ExitPlanMode {
        tool_call_id: String,
        decision: ExitPlanModeDecision,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        feedback: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitPlanModeDecision {
    Approve,
    Reject,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueuedMessageEntry {
    pub id: QueuedMessageId,
    pub message: String,
    pub images: Vec<ImageData>,
    #[serde(default)]
    pub origin: Option<MessageOrigin>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueuedMessagesPayload {
    pub messages: Vec<QueuedMessageEntry>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EditQueuedMessagePayload {
    pub id: QueuedMessageId,
    pub message: String,
    pub images: Vec<ImageData>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CancelQueuedMessagePayload {
    pub id: QueuedMessageId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendQueuedMessageNowPayload {
    pub id: QueuedMessageId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetAgentNamePayload {
    pub name: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentCompactPayload {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary_prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_summary_bytes: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentCompactStatus {
    Started,
    Completed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentCompactNotifyPayload {
    pub status: AgentCompactStatus,
    pub old_agent_id: AgentId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_session_id: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_agent_id: Option<AgentId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_session_id: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary_preview: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InterruptPayload {}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CloseAgentPayload {}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LoadAgentPayload {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FetchSessionHistoryPayload {
    pub agent_id: AgentId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_seq: Option<u64>,
    pub limit: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionHistoryPayload {
    pub agent_id: AgentId,
    pub events: Vec<ChatEvent>,
    pub has_more_before: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oldest_seq: Option<u64>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionListGeneration(pub u64);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SessionListCursor {
    pub generation: SessionListGeneration,
    pub offset: u32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionListScope {
    RootSessions,
    #[default]
    AllSessions,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionListPageStatus {
    #[default]
    Complete,
    More {
        next_cursor: SessionListCursor,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionListPageInfo {
    #[serde(default)]
    pub scope: SessionListScope,
    pub cursor: SessionListCursor,
    pub limit: u32,
    pub total_count: u32,
    pub status: SessionListPageStatus,
}

impl Default for SessionListPageInfo {
    fn default() -> Self {
        Self {
            scope: SessionListScope::AllSessions,
            cursor: SessionListCursor::default(),
            limit: DEFAULT_SESSION_LIST_PAGE_LIMIT,
            total_count: 0,
            status: SessionListPageStatus::Complete,
        }
    }
}

impl SessionListPageInfo {
    pub fn next_cursor(&self) -> Option<SessionListCursor> {
        match self.status {
            SessionListPageStatus::Complete => None,
            SessionListPageStatus::More { next_cursor } => Some(next_cursor),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListSessionsPayload {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<SessionListScope>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<SessionListCursor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeleteSessionPayload {
    pub session_id: SessionId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: SessionId,
    pub backend_kind: BackendKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_profile_id: Option<LaunchProfileId>,
    pub workspace_roots: Vec<String>,
    pub project_id: Option<ProjectId>,
    pub alias: Option<String>,
    pub user_alias: Option<String>,
    pub parent_id: Option<SessionId>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub message_count: u32,
    pub token_count: Option<u64>,
    pub resumable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compacted_from_session_id: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compacted_to_session_id: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compacted_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction_summary_preview: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionListPayload {
    pub sessions: Vec<SessionSummary>,
    pub page: SessionListPageInfo,
}

/// Input events that can be sent to a running agent.
/// This is the typed contract between the connection handler and the agent actor.
/// Variants will grow as agent capabilities expand (cancel, interrupt, etc).
#[derive(Debug, Clone)]
pub enum AgentInput {
    SendMessage(SendMessagePayload),
    EditQueuedMessage(EditQueuedMessagePayload),
    CancelQueuedMessage(CancelQueuedMessagePayload),
    SendQueuedMessageNow(SendQueuedMessageNowPayload),
    UpdateSessionSettings(SetSessionSettingsPayload),
}

// ── Session settings ───────────────────────────────────────────────────

/// Schema describing one backend's configurable session settings.
/// The frontend auto-generates UI controls from this.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSettingsSchema {
    pub backend_kind: BackendKind,
    pub fields: Vec<SessionSettingField>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SessionSchemaEntry {
    Ready {
        schema: SessionSettingsSchema,
    },
    Pending {
        backend_kind: BackendKind,
    },
    Unavailable {
        backend_kind: BackendKind,
        message: String,
    },
}

impl SessionSchemaEntry {
    pub fn backend_kind(&self) -> BackendKind {
        match self {
            Self::Ready { schema } => schema.backend_kind,
            Self::Pending { backend_kind } | Self::Unavailable { backend_kind, .. } => {
                *backend_kind
            }
        }
    }

    pub fn ready_schema(&self) -> Option<&SessionSettingsSchema> {
        match self {
            Self::Ready { schema } => Some(schema),
            Self::Pending { .. } | Self::Unavailable { .. } => None,
        }
    }
}

/// One configurable field in a backend's session settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSettingField {
    /// Machine-readable key, e.g. "model", "reasoning_effort".
    pub key: String,
    /// Human-readable label for the UI.
    pub label: String,
    /// Optional description shown as tooltip or help text.
    pub description: Option<String>,
    /// The type and constraints of this field.
    pub field_type: SessionSettingFieldType,
    /// For Select fields: render as a horizontal slider instead of a dropdown.
    /// Options are treated as ordered positions (low→high). Defaults to false.
    #[serde(default)]
    pub use_slider: bool,
    /// Select-option overrides keyed by another setting's selected value.
    /// The options in `field_type` apply while that setting is unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub select_options_by_setting: Option<SelectOptionsBySetting>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectOptionsBySetting {
    pub setting_key: String,
    pub values: Vec<SelectOptionsForValue>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectOptionsForValue {
    pub setting_value: String,
    pub options: Vec<SelectOption>,
}

impl SessionSettingField {
    pub fn select_options<'a>(
        &'a self,
        values: &'a SessionSettingsValues,
    ) -> Option<&'a [SelectOption]> {
        let SessionSettingFieldType::Select { options, .. } = &self.field_type else {
            return None;
        };
        let Some(options_by_setting) = self.select_options_by_setting.as_ref() else {
            return Some(options);
        };
        match values.0.get(&options_by_setting.setting_key) {
            Some(SessionSettingValue::String(setting_value)) => options_by_setting
                .values
                .iter()
                .find(|entry| entry.setting_value == *setting_value)
                .map(|entry| entry.options.as_slice()),
            Some(SessionSettingValue::Null) | None => Some(options),
            Some(SessionSettingValue::Bool(_) | SessionSettingValue::Integer(_)) => None,
        }
    }
}

/// The type of a session setting field. Determines how the frontend renders it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionSettingFieldType {
    Select {
        options: Vec<SelectOption>,
        default: Option<String>,
        nullable: bool,
    },
    Toggle {
        default: bool,
    },
    Integer {
        min: i64,
        max: i64,
        step: i64,
        default: i64,
    },
}

/// One option in a Select field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectOption {
    pub value: String,
    pub label: String,
}

/// A single session setting value. Typed enum — not serde_json::Value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SessionSettingValue {
    String(String),
    Bool(bool),
    Integer(i64),
    Null,
}

/// Current session settings values for an agent.
/// Keys match `SessionSettingField.key` from the schema.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionSettingsValues(pub HashMap<String, SessionSettingValue>);

/// Server → Client on host stream.
/// Carries session settings schemas for all enabled backends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSchemasPayload {
    pub schemas: Vec<SessionSchemaEntry>,
}

/// Client → Server on agent stream.
/// Partial update: only keys present are changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetSessionSettingsPayload {
    pub values: SessionSettingsValues,
}

/// Server → Client on agent stream.
/// Full effective session settings snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSettingsPayload {
    pub values: SessionSettingsValues,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentStartPayload {
    pub agent_id: AgentId,
    pub name: String,
    pub origin: AgentOrigin,
    pub backend_kind: BackendKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_profile_id: Option<LaunchProfileId>,
    pub workspace_roots: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_agent_id: Option<CustomAgentId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team_id: Option<TeamId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team_member_id: Option<TeamMemberId>,
    pub project_id: Option<ProjectId>,
    pub parent_agent_id: Option<AgentId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<AgentWorkflowMetadata>,
    pub created_at_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRenamedPayload {
    pub agent_id: AgentId,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentClosedPayload {
    pub agent_id: AgentId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentActivitySummary {
    pub text: String,
    pub generated_at_ms: u64,
    pub source_from_seq: Option<u64>,
    pub source_through_seq: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentActivitySummaryStaleReason {
    NewActivity,
    MaxAge,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentActivitySummaryState {
    #[default]
    Disabled,
    Empty,
    Pending {
        requested_at_ms: u64,
        previous: Option<AgentActivitySummary>,
    },
    Fresh {
        summary: AgentActivitySummary,
    },
    Stale {
        summary: AgentActivitySummary,
        reason: AgentActivitySummaryStaleReason,
    },
    Error {
        message: String,
        occurred_at_ms: u64,
        previous: Option<AgentActivitySummary>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentActivitySummaryPayload {
    pub agent_id: AgentId,
    pub state: AgentActivitySummaryState,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentActivityStats {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_output_line: Option<String>,
    #[serde(default)]
    pub tool_calls: u64,
    #[serde(default)]
    pub token_usage: TokenUsage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_through_seq: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentActivityStatsPayload {
    pub agent_id: AgentId,
    pub stats: AgentActivityStats,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskTokenUsagePayload {
    pub root_agent_id: AgentId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_session_id: Option<SessionId>,
    pub total: TaskTokenUsageAggregate,
    pub self_usage: TaskTokenUsageScope,
    pub descendant_usage: TaskTokenUsageAggregate,
    pub descendant_count: u32,
    pub breakdown: Vec<TaskTokenUsageEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskTokenUsageAggregate {
    pub usage: TaskTokenUsageAmount,
    pub status: TaskTokenUsageStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskTokenUsageEntry {
    pub agent_id: AgentId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_agent_id: Option<AgentId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<SessionId>,
    pub name: String,
    pub origin: AgentOrigin,
    pub backend_kind: BackendKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub depth: u32,
    pub tree_index: u32,
    pub usage: TaskTokenUsageScope,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskTokenUsageAmount {
    pub total_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_prompt_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
}

impl TaskTokenUsageAmount {
    pub fn zero() -> Self {
        Self {
            total_tokens: 0,
            input_tokens: Some(0),
            output_tokens: Some(0),
            cached_prompt_tokens: Some(0),
            cache_creation_input_tokens: Some(0),
            reasoning_tokens: Some(0),
        }
    }

    pub fn from_token_usage(usage: &TokenUsage) -> Self {
        Self {
            total_tokens: usage.total_tokens,
            input_tokens: Some(usage.input_tokens),
            output_tokens: Some(usage.output_tokens),
            cached_prompt_tokens: usage.cached_prompt_tokens,
            cache_creation_input_tokens: usage.cache_creation_input_tokens,
            reasoning_tokens: usage.reasoning_tokens,
        }
    }

    pub fn total_only(total_tokens: u64) -> Self {
        Self {
            total_tokens,
            input_tokens: None,
            output_tokens: None,
            cached_prompt_tokens: None,
            cache_creation_input_tokens: None,
            reasoning_tokens: None,
        }
    }

    pub fn saturating_add(&mut self, other: &Self) {
        self.total_tokens = self.total_tokens.saturating_add(other.total_tokens);
        add_optional_usage_amount(&mut self.input_tokens, other.input_tokens);
        add_optional_usage_amount(&mut self.output_tokens, other.output_tokens);
        add_optional_usage_amount(&mut self.cached_prompt_tokens, other.cached_prompt_tokens);
        add_optional_usage_amount(
            &mut self.cache_creation_input_tokens,
            other.cache_creation_input_tokens,
        );
        add_optional_usage_amount(&mut self.reasoning_tokens, other.reasoning_tokens);
    }
}

fn add_optional_usage_amount(total: &mut Option<u64>, value: Option<u64>) {
    *total = match (*total, value) {
        (Some(left), Some(right)) => Some(left.saturating_add(right)),
        _ => None,
    };
}

impl Default for TaskTokenUsageAmount {
    fn default() -> Self {
        Self::zero()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TaskTokenUsageScope {
    Known {
        usage: Box<TaskTokenUsageAmount>,
    },
    Partial {
        usage: Box<TaskTokenUsageAmount>,
        unavailable_count: u32,
        reasons: Vec<TaskTokenUsageUnavailableReason>,
    },
    Unavailable {
        reason: TaskTokenUsageUnavailableReason,
    },
}

impl TaskTokenUsageScope {
    pub fn known_usage(&self) -> Option<&TaskTokenUsageAmount> {
        match self {
            Self::Known { usage } => Some(usage),
            Self::Partial { .. } | Self::Unavailable { .. } => None,
        }
    }

    pub fn reported_usage(&self) -> Option<&TaskTokenUsageAmount> {
        match self {
            Self::Known { usage } | Self::Partial { usage, .. } => Some(usage),
            Self::Unavailable { .. } => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TaskTokenUsageStatus {
    Known,
    Partial {
        unavailable_count: u32,
        reasons: Vec<TaskTokenUsageUnavailableReason>,
    },
    Unavailable {
        unavailable_count: u32,
        reasons: Vec<TaskTokenUsageUnavailableReason>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskTokenUsageUnavailableReason {
    NoAssistantTurnCompleted,
    BackendDidNotReport,
    ProviderScopeAmbiguous,
    AgentUnavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewAgentPayload {
    pub agent_id: AgentId,
    pub name: String,
    pub origin: AgentOrigin,
    pub backend_kind: BackendKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_profile_id: Option<LaunchProfileId>,
    pub workspace_roots: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_agent_id: Option<CustomAgentId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team_id: Option<TeamId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team_member_id: Option<TeamMemberId>,
    pub project_id: Option<ProjectId>,
    pub parent_agent_id: Option<AgentId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<AgentWorkflowMetadata>,
    pub created_at_ms: u64,
    pub instance_stream: StreamPath,
    #[serde(default)]
    pub activity_summary: AgentActivitySummaryState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CustomAgent {
    pub id: CustomAgentId,
    pub name: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    #[serde(default)]
    pub skill_ids: Vec<SkillId>,
    #[serde(default)]
    pub mcp_server_ids: Vec<McpServerId>,
    pub tool_policy: ToolPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolPolicy {
    Unrestricted,
    AllowList { tools: Vec<String> },
    DenyList { tools: Vec<String> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Steering {
    pub id: SteeringId,
    pub scope: SteeringScope,
    pub title: String,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SteeringScope {
    Host,
    Project(ProjectId),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Skill {
    pub id: SkillId,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerConfig {
    pub id: McpServerId,
    pub name: String,
    pub transport: McpTransportConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum McpTransportConfig {
    Http {
        url: String,
        #[serde(default)]
        headers: HashMap<String, String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bearer_token_env_var: Option<String>,
    },
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: HashMap<String, String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CustomAgentUpsertPayload {
    pub custom_agent: CustomAgent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CustomAgentDeletePayload {
    pub id: CustomAgentId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SteeringUpsertPayload {
    pub steering: Steering,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SteeringDeletePayload {
    pub id: SteeringId,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillRefreshPayload {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerUpsertPayload {
    pub mcp_server: McpServerConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerDeletePayload {
    pub id: McpServerId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CustomAgentNotifyPayload {
    Upsert { custom_agent: CustomAgent },
    Delete { id: CustomAgentId },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SteeringNotifyPayload {
    Upsert { steering: Steering },
    Delete { id: SteeringId },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SkillNotifyPayload {
    Upsert { skill: Skill },
    Delete { id: SkillId },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum McpServerNotifyPayload {
    Upsert { mcp_server: McpServerConfig },
    Delete { id: McpServerId },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeamMemberRole {
    Manager,
    Report,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeamMemberState {
    Active,
    Paused,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeamPersonalityTrait {
    Cautious,
    Pragmatic,
    Bold,
    Contrarian,
    Terse,
    Conversational,
    Pedagogical,
    Skeptical,
    RefactorLeaning,
    ShipIt,
    TestFirst,
    TypeSystem,
    Yagni,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamMemberPresetProfile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role_preset_id: Option<TeamRolePresetId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub personality_preset_id: Option<TeamPersonalityPresetId>,
    #[serde(default)]
    pub personality_traits: Vec<TeamPersonalityTrait>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamRolePreset {
    pub id: TeamRolePresetId,
    pub name: String,
    pub summary: String,
    pub default_member_name: String,
    pub default_description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_custom_agent_id: Option<CustomAgentId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamPersonalityTraitPreset {
    pub trait_id: TeamPersonalityTrait,
    pub name: String,
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamPersonalityPreset {
    pub id: TeamPersonalityPresetId,
    pub name: String,
    pub summary: String,
    pub traits: Vec<TeamPersonalityTrait>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamTemplateMember {
    pub org_role: TeamMemberRole,
    pub role_preset_id: TeamRolePresetId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub personality_preset_id: Option<TeamPersonalityPresetId>,
    pub name: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamTemplate {
    pub id: TeamTemplateId,
    pub name: String,
    pub summary: String,
    pub balanced: bool,
    pub members: Vec<TeamTemplateMember>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamPresetCatalog {
    pub role_presets: Vec<TeamRolePreset>,
    pub personality_traits: Vec<TeamPersonalityTraitPreset>,
    pub personality_presets: Vec<TeamPersonalityPreset>,
    pub team_templates: Vec<TeamTemplate>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamDraftMember {
    pub id: TeamDraftMemberId,
    pub org_role: TeamMemberRole,
    pub name: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<TeamMemberPresetProfile>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_agent_id: Option<CustomAgentId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend_kind: Option<BackendKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_hint: Option<SpawnCostHint>,
    #[serde(default)]
    pub project_ids: Vec<ProjectId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamDraft {
    pub id: TeamDraftId,
    pub name: String,
    pub members: Vec<TeamDraftMember>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Team {
    pub id: TeamId,
    pub name: String,
    pub manager_member_id: TeamMemberId,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamMember {
    pub id: TeamMemberId,
    pub team_id: TeamId,
    pub role: TeamMemberRole,
    pub state: TeamMemberState,
    pub name: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<TeamMemberPresetProfile>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_agent_id: Option<CustomAgentId>,
    pub backend_kind: BackendKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_hint: Option<SpawnCostHint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    pub project_ids: Vec<ProjectId>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamMemberBindingPayload {
    pub member_id: TeamMemberId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_agent_id: Option<AgentId>,
    pub status: AgentControlStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_active_at_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamMemberCreateSpec {
    pub name: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<TeamMemberPresetProfile>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_agent_id: Option<CustomAgentId>,
    pub backend_kind: BackendKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_hint: Option<SpawnCostHint>,
    pub project_ids: Vec<ProjectId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamCreatePayload {
    pub name: String,
    pub manager: TeamMemberCreateSpec,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamCreateFromDraftPayload {
    pub name: String,
    pub manager: TeamMemberCreateSpec,
    pub reports: Vec<TeamMemberCreateSpec>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamRenamePayload {
    pub id: TeamId,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamDeletePayload {
    pub id: TeamId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamSetManagerPayload {
    pub team_id: TeamId,
    pub new_manager_member_id: TeamMemberId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamMemberCreatePayload {
    pub team_id: TeamId,
    pub member: TeamMemberCreateSpec,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamMemberUpdatePayload {
    pub id: TeamMemberId,
    pub name: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<TeamMemberPresetProfile>,
    pub project_ids: Vec<ProjectId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamMemberDeletePayload {
    pub id: TeamMemberId,
}

/// User-initiated team-member activation, sent from the frontend on the host
/// stream. Mirrors the manager-initiated `tyde_team_message_member` flow but
/// has no caller agent (the user is the caller). `prompt: None` is the
/// "just open the chat" case: if the member has no live binding and no
/// session, the server does nothing — activation defers until the user types
/// a first message and re-sends with `prompt: Some`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TeamMemberActivatePayload {
    pub member_id: TeamMemberId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub images: Option<Vec<ImageData>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamCompactPayload {
    pub team_id: TeamId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary_prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_summary_bytes: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeamCompactStatus {
    Started,
    Completed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamCompactNotifyPayload {
    pub status: TeamCompactStatus,
    pub team_id: TeamId,
    #[serde(default)]
    pub member_ids: Vec<TeamMemberId>,
    #[serde(default)]
    pub agent_ids: Vec<AgentId>,
    #[serde(default)]
    pub results: Vec<AgentCompactNotifyPayload>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TeamNotifyPayload {
    Upsert { team: Team },
    Delete { team: Team },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TeamMemberNotifyPayload {
    Upsert { member: TeamMember },
    Delete { member: TeamMember },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TeamMemberBindingNotifyPayload {
    Upsert { binding: TeamMemberBindingPayload },
    Delete { binding: TeamMemberBindingPayload },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamPresetCatalogNotifyPayload {
    pub catalog: TeamPresetCatalog,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TeamDraftNotifyPayload {
    Upsert { draft: TeamDraft },
    Delete { draft_id: TeamDraftId },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamDraftCreatePayload {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template_id: Option<TeamTemplateId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TeamDraftUpdatePayload {
    SetName {
        draft_id: TeamDraftId,
        name: String,
    },
    ReplaceMember {
        draft_id: TeamDraftId,
        member: TeamDraftMemberEdit,
    },
    AddReport {
        draft_id: TeamDraftId,
    },
    RemoveMember {
        draft_id: TeamDraftId,
        member_id: TeamDraftMemberId,
    },
    SetMemberProfile {
        draft_id: TeamDraftId,
        member_id: TeamDraftMemberId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        role_preset_id: Option<TeamRolePresetId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        personality_preset_id: Option<TeamPersonalityPresetId>,
        #[serde(default)]
        personality_traits: Vec<TeamPersonalityTrait>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeamDraftShuffleScope {
    Member,
    Personality,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamDraftShufflePayload {
    pub draft_id: TeamDraftId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub member_id: Option<TeamDraftMemberId>,
    pub scope: TeamDraftShuffleScope,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamDraftApplyTemplatePayload {
    pub draft_id: TeamDraftId,
    pub template_id: TeamTemplateId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamDraftCommitPayload {
    pub draft_id: TeamDraftId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamDraftDiscardPayload {
    pub draft_id: TeamDraftId,
}

/// Editable fields the frontend may change on a draft member via
/// `TeamDraftUpdate::ReplaceMember`. Server-owned fields (`id`, `org_role`,
/// `profile`) are intentionally absent: those move through dedicated
/// updates (`SetMemberProfile`, etc.) so the client cannot mutate them
/// behind the registry's back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamDraftMemberEdit {
    pub id: TeamDraftMemberId,
    pub name: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_agent_id: Option<CustomAgentId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend_kind: Option<BackendKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_hint: Option<SpawnCostHint>,
    #[serde(default)]
    pub project_ids: Vec<ProjectId>,
}

/// User-driven request to shuffle a candidate member profile when adding a
/// new report to an existing team. The server picks a random role and
/// personality from its catalog and emits a `TeamMemberShuffleSuggestion`
/// notify; the frontend then applies the suggestion to the open Add-report
/// form. This keeps semantic preset selection on the server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamMemberShufflePayload {
    pub team_id: TeamId,
}

/// Server-emitted suggestion for an Add-report shuffle. The frontend
/// applies these fields to the open dialog's editable form signals.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamMemberShuffleSuggestion {
    pub name: String,
    pub description: String,
    pub profile: TeamMemberPresetProfile,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_agent_id: Option<CustomAgentId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamMemberShuffleSuggestionNotifyPayload {
    pub team_id: TeamId,
    pub suggestion: TeamMemberShuffleSuggestion,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    pub id: ProjectId,
    pub name: String,
    #[serde(default)]
    pub sort_order: u64,
    pub source: ProjectSource,
}

impl Project {
    pub fn root_paths(&self) -> Vec<ProjectRootPath> {
        match &self.source {
            ProjectSource::Standalone { roots } => roots.clone(),
            ProjectSource::GitWorkbench { roots, .. } => roots
                .iter()
                .map(|root| root.worktree_root.clone())
                .collect(),
        }
    }

    pub fn parent_project_id(&self) -> Option<&ProjectId> {
        match &self.source {
            ProjectSource::Standalone { .. } => None,
            ProjectSource::GitWorkbench {
                parent_project_id, ..
            } => Some(parent_project_id),
        }
    }

    pub fn is_workbench(&self) -> bool {
        matches!(self.source, ProjectSource::GitWorkbench { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProjectSource {
    Standalone {
        roots: Vec<ProjectRootPath>,
    },
    GitWorkbench {
        parent_project_id: ProjectId,
        branch: GitBranchName,
        roots: Vec<WorkbenchRoot>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkbenchRoot {
    pub parent_root: ProjectRootPath,
    pub worktree_root: ProjectRootPath,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectCreatePayload {
    pub name: String,
    pub roots: Vec<ProjectRootPath>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectRenamePayload {
    pub id: ProjectId,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProjectReorderScope {
    TopLevel,
    WorkbenchChildren { parent_project_id: ProjectId },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectReorderPayload {
    pub scope: ProjectReorderScope,
    pub project_ids: Vec<ProjectId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectAddRootPayload {
    pub id: ProjectId,
    pub root: ProjectRootPath,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectDeleteRootPayload {
    pub id: ProjectId,
    pub root: ProjectRootPath,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectDeletePayload {
    pub id: ProjectId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProjectNotifyPayload {
    Upsert { project: Project },
    Delete { project: Project },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkbenchCreatePayload {
    pub parent_project_id: ProjectId,
    pub branch: GitBranchName,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkbenchRemovePayload {
    pub id: ProjectId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProjectEventPayload {
    ReviewListChanged {
        reviews: Vec<ReviewSummary>,
    },
    /// One or more files advanced their centralized version because a change
    /// reached the filesystem watcher (external edit, agent write, branch
    /// switch, save-on-format, …). The frontend re-reads any of these it
    /// currently has open so its rendered version — and thus the version it
    /// stamps onto code-intel queries — tracks the server's instead of
    /// freezing at open time. Without this, a subscribed file's server-side
    /// version races ahead on every watch event while the client stays pinned
    /// to the version it opened at, so every hover / go-to-def / find-refs is
    /// rejected as `stale code-intel request` until the file is manually
    /// reopened.
    FilesChanged {
        files: Vec<ProjectFileVersionChange>,
    },
}

/// A single per-file version advance carried to the frontend on
/// [`ProjectEventPayload::FilesChanged`]. This is the wire mirror of the
/// server-internal `FileVersionChange`: "the file at `path` is now at
/// `version`".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectFileVersionChange {
    pub path: ProjectPath,
    pub version: ProjectFileVersion,
}

#[derive(
    Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(transparent)]
pub struct ProjectRootPath(pub String);

impl fmt::Display for ProjectRootPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct GitBranchName(pub String);

impl fmt::Display for GitBranchName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProjectPath {
    pub root: ProjectRootPath,
    pub relative_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectReadFilePayload {
    pub path: ProjectPath,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectDiffScope {
    Unstaged,
    Staged,
    /// `git diff HEAD` — staged + unstaged combined. Legacy Review records
    /// may still deserialize with this scope, but active inline reviews use
    /// `Unstaged`.
    Uncommitted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffContextMode {
    Hunks,
    FullFile,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectReadDiffPayload {
    pub root: ProjectRootPath,
    pub scope: ProjectDiffScope,
    pub path: Option<String>,
    pub context_mode: DiffContextMode,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectStageFilePayload {
    pub path: ProjectPath,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectStageHunkPayload {
    pub path: ProjectPath,
    pub hunk_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectUnstageFilePayload {
    pub path: ProjectPath,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectDiscardFilePayload {
    pub path: ProjectPath,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectGitCommitPayload {
    pub root: ProjectRootPath,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectGitCommitResultPayload {
    pub root: ProjectRootPath,
    pub commit_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectListDirPayload {
    pub root: ProjectRootPath,
    /// Relative path of the directory to list. Empty string means root.
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectFileListPayload {
    #[serde(default)]
    pub incremental: bool,
    pub roots: Vec<ProjectRootListing>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectRootListing {
    pub root: ProjectRootPath,
    pub entries: Vec<ProjectFileEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectFileEntry {
    pub relative_path: String,
    pub kind: ProjectFileKind,
    pub op: FileEntryOp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileEntryOp {
    Add,
    Remove,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectFileKind {
    File,
    Directory,
    Symlink,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectGitStatusPayload {
    pub roots: Vec<ProjectRootGitStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectRootGitStatus {
    pub root: ProjectRootPath,
    pub branch: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub clean: bool,
    pub files: Vec<ProjectGitFileStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectGitFileStatus {
    pub relative_path: String,
    pub staged: Option<ProjectGitChangeKind>,
    pub unstaged: Option<ProjectGitChangeKind>,
    pub untracked: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectGitChangeKind {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    TypeChanged,
}

/// Monotonic per-file version counter, owned by the project-stream actor. Each
/// file read, filesystem-watcher change, and agent write bumps the **same**
/// counter for that file. Every [`ProjectFileContentsPayload`] and every
/// `CodeIntel*` frame carries the version of the contents it describes so the
/// client can apply semantic decorations only against the matching text (see
/// `dev-docs/24-code-intelligence.md` §2.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProjectFileVersion(pub u64);

impl fmt::Display for ProjectFileVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectFileContentsPayload {
    pub path: ProjectPath,
    pub version: ProjectFileVersion,
    pub contents: Option<String>,
    pub is_binary: bool,
    /// The file did not exist on disk when this read ran. Server-owned
    /// existence signal for open viewers: a watcher-driven refresh of a
    /// deleted file reports `missing: true` (with `contents: None`) instead of
    /// a pathless command error, so the client can label the exact viewer
    /// "deleted on disk" without inferring deletion from directory listings.
    #[serde(default)]
    pub missing: bool,
}

// ── Project global search ─────────────────────────────────────────────────

/// Client → Server request to run a project-wide text search. Results stream
/// back as one [`ProjectSearchResultsPayload`] per matching file, terminated
/// by a single [`ProjectSearchCompletePayload`]. Searches are identified by a
/// client-chosen, monotonically increasing `search_id`; a newer search (or a
/// matching [`ProjectSearchCancelPayload`]) supersedes any in-flight walk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectSearchPayload {
    pub search_id: u64,
    pub query: String,
    #[serde(default)]
    pub case_sensitive: bool,
    #[serde(default)]
    pub whole_word: bool,
    #[serde(default)]
    pub use_regex: bool,
    /// When true, gitignored / hidden files are also searched.
    #[serde(default)]
    pub include_ignored: bool,
    /// Roots to search. Empty means "all of the project's roots".
    #[serde(default)]
    pub roots: Vec<ProjectRootPath>,
    /// Optional relative-path prefix used to scope the search to a folder
    /// (the "search in folder" action). Matched against the root-relative
    /// path of each file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path_prefix: Option<String>,
    /// Optional override for the maximum number of matching files to return.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_results: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectSearchCancelPayload {
    pub search_id: u64,
}

/// Client → Server notification that the project backing this `/project/<id>`
/// stream was selected/accessed by the user. The project id is carried by the
/// stream path, not duplicated in the payload.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectAccessedPayload {}

/// A single matching line within a file. `ranges` are byte offsets into
/// `line_text` (which the server sends verbatim) so the client can slice the
/// exact same bytes when highlighting — no UTF-8/UTF-16 mismatch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectSearchMatch {
    /// 1-based line number.
    pub line_number: u32,
    pub line_text: String,
    pub ranges: Vec<(u32, u32)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectSearchFileResult {
    pub path: ProjectPath,
    pub matches: Vec<ProjectSearchMatch>,
    /// True when the per-file match cap was hit and some matches were dropped.
    pub truncated: bool,
}

/// Server → Client: one matching file's results. Streamed incrementally.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectSearchResultsPayload {
    pub search_id: u64,
    pub file: ProjectSearchFileResult,
}

/// Server → Client: terminal frame for a search. Carries the final totals and
/// whether the walk was truncated (caps hit), cancelled, or errored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectSearchCompletePayload {
    pub search_id: u64,
    pub total_files: u32,
    pub total_matches: u32,
    pub truncated: bool,
    pub cancelled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// ── Code intelligence ─────────────────────────────────────────────────────
//
// Server-owned code intelligence (go-to-definition, hover, diagnostics,
// find-references). These frames ride the existing `/project/<project_id>`
// stream. Positions on the wire are **byte offsets** into the file contents at
// the carried `ProjectFileVersion`; UTF-16 conversion is confined to the
// rust-analyzer provider, server-side. See `dev-docs/24-code-intelligence.md`.

/// Open language identifier on the wire — NOT a closed enum. Adding pyright /
/// gopls adds no protocol variant. The closed server-side `Language` enum lives
/// in the server only; the frontend treats this as an opaque display label.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CodeIntelLanguageId(pub String);

impl fmt::Display for CodeIntelLanguageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Open provider identifier on the wire — NOT a closed enum (e.g.
/// "rust-analyzer", "pyright"). Rendered as an opaque label by the frontend.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CodeIntelProviderId(pub String);

impl fmt::Display for CodeIntelProviderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Shared half-open byte range `[start, end)` into a file or a line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ByteRange {
    /// Inclusive byte offset.
    pub start: u32,
    /// Exclusive byte offset.
    pub end: u32,
}

// ── Code-intel: status (server → client) ──────────────────────────────────

/// Tagged scope that carries identity, so the UI knows *which* provider/file a
/// status pertains to — not just *that* something changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CodeIntelStatusScope {
    Project,
    Provider {
        root: ProjectRootPath,
    },
    File {
        path: ProjectPath,
        version: ProjectFileVersion,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodeIntelState {
    /// No provider matches this language.
    Unsupported,
    /// A provider exists but the backing binary is absent.
    Unavailable,
    Starting,
    Indexing,
    Ready,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodeIntelResourceMode {
    Full,
    Limited,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeIntelProviderStatus {
    pub provider: CodeIntelProviderId,
    pub language: CodeIntelLanguageId,
    pub state: CodeIntelState,
    pub resource_mode: CodeIntelResourceMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_done: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_work: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Files-with-diagnostics aggregate for this provider's workspace: total
    /// error diagnostics across *all* files the server has published for
    /// (open or not). The server owns this because the client drops
    /// diagnostics for closed files.
    #[serde(default)]
    pub error_count: u32,
    /// Same aggregate for warnings.
    #[serde(default)]
    pub warning_count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeIntelRootOverview {
    pub root: ProjectRootPath,
    pub providers: Vec<CodeIntelProviderStatus>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodeIntelOverviewHeadline {
    NotStarted,
    Starting,
    Indexing,
    Ready,
    Unavailable,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeIntelOverviewSummary {
    pub headline: CodeIntelOverviewHeadline,
    pub ready: u32,
    pub indexing: u32,
    pub starting: u32,
    pub unavailable: u32,
    pub failed: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Project-wide diagnostics aggregate: error diagnostics summed over every
    /// provider (which each count across all their workspace files, open or
    /// not). Server-owned so the footer can show real error visibility even
    /// though the client drops closed-file diagnostics.
    #[serde(default)]
    pub error_count: u32,
    /// Same aggregate for warnings.
    #[serde(default)]
    pub warning_count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeIntelOverviewPayload {
    pub roots: Vec<CodeIntelRootOverview>,
    pub summary: CodeIntelOverviewSummary,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeIntelStatusPayload {
    pub scope: CodeIntelStatusScope,
    pub state: CodeIntelState,
    pub resource_mode: CodeIntelResourceMode,
    /// Present while indexing; mapped from RA `$/progress`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_done: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_work: Option<u32>,
    /// Human-readable hint, e.g. "rustup component add rust-analyzer".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

// ── Code-intel: input events (client → server) ─────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeIntelSubscribeFilePayload {
    pub path: ProjectPath,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeIntelUnsubscribeFilePayload {
    pub path: ProjectPath,
}

/// Pure prioritization hint. Never gates which identifiers are clickable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeIntelSetVisibleRangePayload {
    pub path: ProjectPath,
    pub version: ProjectFileVersion,
    pub range: ByteRange,
}

/// On-demand hover. `hover_id` is a client-chosen domain id (cf. `search_id`)
/// that correlates the streamed result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeIntelHoverPayload {
    pub hover_id: u64,
    pub path: ProjectPath,
    pub version: ProjectFileVersion,
    /// Byte offset into the file.
    pub offset: u32,
}

/// Miss-fill for a click whose target has not been pushed yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeIntelNavigatePayload {
    pub navigate_id: u64,
    pub path: ProjectPath,
    pub version: ProjectFileVersion,
    pub offset: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeIntelFindReferencesPayload {
    /// Domain id, like `search_id`.
    pub references_id: u64,
    pub path: ProjectPath,
    pub version: ProjectFileVersion,
    /// The symbol to find references to.
    pub offset: u32,
    pub include_declaration: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeIntelCancelReferencesPayload {
    pub references_id: u64,
}

// ── Code-intel: file model (server → client) ───────────────────────────────

/// Progressive coverage of the file, NOT a permanent range gate. A `ByteRange`
/// with `completeness: Partial` is a transient chunk on the way to an eventual
/// `FullFile` + `Complete` model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CodeIntelModelRange {
    FullFile,
    ByteRange { range: ByteRange },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodeIntelCompleteness {
    /// Whole file resolved: every occurrence has its target(s).
    Complete,
    /// More occurrences/targets still streaming toward `Complete`.
    Partial,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodeIntelRole {
    Definition,
    Reference,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeIntelLocation {
    pub path: ProjectPath,
    pub range: ByteRange,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeIntelOccurrence {
    /// The clickable identifier span.
    pub range: ByteRange,
    pub role: CodeIntelRole,
    /// Short label for tooltip/affordance.
    pub display: String,
    /// Empty until targets stream in; the client merges by `range`. LSP
    /// `textDocument/definition` can return multiple locations, so this is a
    /// list, not a single target.
    #[serde(default)]
    pub definition: Vec<CodeIntelLocation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeIntelFileModelPayload {
    pub path: ProjectPath,
    pub version: ProjectFileVersion,
    pub provider: CodeIntelProviderId,
    pub language: CodeIntelLanguageId,
    pub model_range: CodeIntelModelRange,
    pub completeness: CodeIntelCompleteness,
    pub occurrences: Vec<CodeIntelOccurrence>,
}

// ── Code-intel: diagnostics (server → client) ──────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodeIntelSeverity {
    Error,
    Warning,
    Information,
    Hint,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeIntelDiagnostic {
    pub range: ByteRange,
    pub severity: CodeIntelSeverity,
    pub message: String,
    /// e.g. "rustc", "clippy".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// Full-file replace snapshot of diagnostics, pushed unsolicited.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeIntelDiagnosticsPayload {
    pub path: ProjectPath,
    pub version: ProjectFileVersion,
    /// Replaces the prior set wholesale.
    pub diagnostics: Vec<CodeIntelDiagnostic>,
}

// ── Code-intel: navigate / hover results (server → client) ─────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeIntelNavigateResultPayload {
    pub navigate_id: u64,
    pub path: ProjectPath,
    pub version: ProjectFileVersion,
    /// Empty means "no definition found here" (a valid answer, not an error).
    pub targets: Vec<CodeIntelLocation>,
    /// Definition targets the language server returned that resolve *outside
    /// this provider's workspace root* (standard library, dependencies, or —
    /// in a multi-root project — another root; providers are per-root and do
    /// not classify against sibling roots). They are dropped from `targets`
    /// (not navigable), but the count lets a client explain an
    /// otherwise-silent no-op jump.
    #[serde(default)]
    pub external_targets: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeIntelHoverResultPayload {
    pub hover_id: u64,
    pub path: ProjectPath,
    pub version: ProjectFileVersion,
    /// None means "nothing to show here" (a valid answer, not an error).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contents: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<ByteRange>,
}

// ── Code-intel: find-references (server → client, streamed) ─────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeIntelReferenceLine {
    /// 1-based line number.
    pub line_number: u32,
    /// Sent verbatim.
    pub line_text: String,
    /// Byte ranges into `line_text`.
    pub ranges: Vec<ByteRange>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeIntelReferencesFileResult {
    pub path: ProjectPath,
    pub lines: Vec<CodeIntelReferenceLine>,
    /// Per-file cap hit.
    pub truncated: bool,
}

/// One matching file's references. Streamed incrementally.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeIntelReferencesResultsPayload {
    pub references_id: u64,
    pub file: CodeIntelReferencesFileResult,
}

/// Terminal frame: totals, truncation, cancellation, error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeIntelReferencesCompletePayload {
    pub references_id: u64,
    pub total_files: u32,
    pub total_references: u32,
    pub truncated: bool,
    pub cancelled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// ── Code-intel: errors (server → client) ───────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodeIntelErrorCode {
    /// Binary absent.
    ProviderUnavailable,
    ProviderCrashed,
    UnsupportedLanguage,
    /// Request referenced a version the server no longer holds.
    StaleVersion,
    Timeout,
    /// Malformed LSP traffic from the provider.
    ProtocolError,
    Internal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CodeIntelErrorContext {
    Subscribe {
        path: ProjectPath,
    },
    Hover {
        hover_id: u64,
        path: ProjectPath,
    },
    Navigate {
        navigate_id: u64,
        path: ProjectPath,
    },
    FindReferences {
        references_id: u64,
        path: ProjectPath,
    },
    Provider {
        language: CodeIntelLanguageId,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeIntelErrorPayload {
    pub code: CodeIntelErrorCode,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr: Option<String>,
    pub context: CodeIntelErrorContext,
    pub fatal: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectGitDiffPayload {
    pub root: ProjectRootPath,
    pub scope: ProjectDiffScope,
    pub path: Option<String>,
    pub context_mode: DiffContextMode,
    pub files: Vec<ProjectGitDiffFile>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectGitDiffFile {
    pub relative_path: String,
    #[serde(default)]
    pub is_binary: bool,
    pub hunks: Vec<ProjectGitDiffHunk>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectGitDiffHunk {
    pub hunk_id: String,
    pub old_start: u32,
    pub old_count: u32,
    pub new_start: u32,
    pub new_count: u32,
    pub lines: Vec<ProjectGitDiffLine>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectGitDiffLine {
    pub kind: ProjectGitDiffLineKind,
    pub text: String,
    pub old_line_number: Option<u32>,
    pub new_line_number: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectGitDiffLineKind {
    Context,
    Added,
    Removed,
}

// ── Review ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ReviewStatus {
    /// User editing — comments and AI suggestions can change.
    Draft,
    /// Frozen, accepted comments locked. Bundle queued for delivery; the
    /// originating agent may not be live yet.
    Submitted { submitted_at_ms: u64 },
    /// Bundle delivered to a live agent actor for the originating session.
    Consumed {
        submitted_at_ms: u64,
        consumed_at_ms: u64,
        target_agent_id: AgentId,
    },
    /// Explicit user discard. Terminal.
    Cancelled { cancelled_at_ms: u64 },
}

impl ReviewStatus {
    pub const fn status_label(&self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::Submitted { .. } => "submitted",
            Self::Consumed { .. } => "consumed",
            Self::Cancelled { .. } => "cancelled",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReviewDiffSelection {
    /// Legacy v1 default. New inline reviews are workspace-scoped and normalize to
    /// `Workspace { scope: Unstaged }`.
    AllUncommitted,
    /// All roots in the project workspace.
    Workspace { scope: ProjectDiffScope },
    /// One project root, optionally narrowed to a path.
    Root {
        root: ProjectRootPath,
        scope: ProjectDiffScope,
        path: Option<String>,
    },
}

impl ReviewDiffSelection {
    pub const fn kind_name(&self) -> &'static str {
        match self {
            Self::AllUncommitted => "all_uncommitted",
            Self::Workspace { .. } => "workspace",
            Self::Root { .. } => "root",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ReviewLocation {
    pub root: ProjectRootPath,
    pub relative_path: String,
    pub anchor: ReviewAnchor,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReviewAnchor {
    File,
    Hunk {
        hunk_id: String,
        old_start: u32,
        old_count: u32,
        new_start: u32,
        new_count: u32,
    },
    LineRange {
        side: ReviewDiffSide,
        start_line: u32,
        end_line: u32,
    },
}

impl ReviewAnchor {
    pub const fn kind_name(&self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Hunk { .. } => "hunk",
            Self::LineRange { .. } => "line_range",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReviewDiffSide {
    Old,
    New,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewComment {
    pub id: ReviewCommentId,
    pub location: ReviewLocation,
    #[serde(default)]
    pub anchor_status: ReviewAnchorStatus,
    pub body: String,
    pub source: ReviewCommentSource,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ReviewAnchorStatus {
    #[default]
    Current,
    Stale {
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReviewCommentSource {
    User,
    AiSuggestion {
        suggestion_id: ReviewSuggestionId,
        edited: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewSuggestedComment {
    pub id: ReviewSuggestionId,
    pub location: ReviewLocation,
    #[serde(default)]
    pub anchor_status: ReviewAnchorStatus,
    pub body: String,
    pub rationale: Option<String>,
    pub severity: ReviewSeverity,
    pub state: ReviewSuggestionState,
    pub reviewer_agent_id: AgentId,
    pub created_at_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReviewSeverity {
    Info,
    Warn,
    Bug,
}

impl ReviewSeverity {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Bug => "bug",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ReviewSuggestionState {
    Pending,
    Accepted { comment_id: ReviewCommentId },
    Rejected,
}

impl ReviewSuggestionState {
    pub const fn status_label(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Accepted { .. } => "accepted",
            Self::Rejected => "rejected",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Review {
    pub id: ReviewId,
    pub project_id: ProjectId,
    pub origin_agent_id: AgentId,
    pub origin_session_id: SessionId,
    pub selection: ReviewDiffSelection,
    pub status: ReviewStatus,
    pub diffs: Vec<ProjectGitDiffPayload>,
    pub comments: Vec<ReviewComment>,
    pub suggestions: Vec<ReviewSuggestedComment>,
    pub ai_reviewer: ReviewAiReviewerState,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewAiReviewerState {
    pub status: ReviewAiReviewerStatus,
    pub agent_id: Option<AgentId>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewAiReviewerStatus {
    Idle,
    Running,
    Completed,
    Failed,
}

impl ReviewAiReviewerStatus {
    pub const fn status_label(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewCreatePayload {
    pub selection: ReviewDiffSelection,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewSubscribePayload {
    #[serde(
        default = "default_review_subscribe_include_diffs",
        skip_serializing_if = "is_default_review_subscribe_include_diffs"
    )]
    pub include_diffs: bool,
}

impl Default for ReviewSubscribePayload {
    fn default() -> Self {
        Self {
            include_diffs: true,
        }
    }
}

const fn default_review_subscribe_include_diffs() -> bool {
    true
}

const fn is_default_review_subscribe_include_diffs(value: &bool) -> bool {
    *value
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReviewSubmitTarget {
    ExistingAgent {
        agent_id: AgentId,
    },
    NewAgent {
        backend_kind: BackendKind,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cost_hint: Option<SpawnCostHint>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        custom_agent_id: Option<CustomAgentId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        instructions: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReviewActionPayload {
    AddComment {
        location: ReviewLocation,
        body: String,
    },
    UpdateComment {
        comment_id: ReviewCommentId,
        body: String,
    },
    DeleteComment {
        comment_id: ReviewCommentId,
    },
    AcceptSuggestion {
        suggestion_id: ReviewSuggestionId,
        edit: Option<String>,
    },
    RejectSuggestion {
        suggestion_id: ReviewSuggestionId,
    },
    StartAiReview {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        backend_kind: Option<BackendKind>,
        cost_hint: Option<SpawnCostHint>,
        instructions: Option<String>,
    },
    Submit {
        target: ReviewSubmitTarget,
    },
    ClearComments,
    Cancel,
}

impl ReviewActionPayload {
    pub const fn kind_name(&self) -> &'static str {
        match self {
            Self::AddComment { .. } => "add_comment",
            Self::UpdateComment { .. } => "update_comment",
            Self::DeleteComment { .. } => "delete_comment",
            Self::AcceptSuggestion { .. } => "accept_suggestion",
            Self::RejectSuggestion { .. } => "reject_suggestion",
            Self::StartAiReview { .. } => "start_ai_review",
            Self::Submit { .. } => "submit",
            Self::ClearComments => "clear_comments",
            Self::Cancel => "cancel",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReviewEventPayload {
    Snapshot { review: Review },
    CommentUpsert { comment: ReviewComment },
    CommentDelete { comment_id: ReviewCommentId },
    SuggestionUpsert { suggestion: ReviewSuggestedComment },
    AiReviewerChanged { state: ReviewAiReviewerState },
    StatusChanged { status: ReviewStatus },
    Cleared { review: Review },
    Error { error: ReviewErrorPayload },
}

impl ReviewEventPayload {
    pub const fn kind_name(&self) -> &'static str {
        match self {
            Self::Snapshot { .. } => "snapshot",
            Self::CommentUpsert { .. } => "comment_upsert",
            Self::CommentDelete { .. } => "comment_delete",
            Self::SuggestionUpsert { .. } => "suggestion_upsert",
            Self::AiReviewerChanged { .. } => "ai_reviewer_changed",
            Self::StatusChanged { .. } => "status_changed",
            Self::Cleared { .. } => "cleared",
            Self::Error { .. } => "error",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewErrorPayload {
    pub code: ReviewErrorCode,
    pub message: String,
    pub fatal: bool,
    pub context: ReviewErrorContext,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewErrorCode {
    InvalidStatus,
    InvalidLocation,
    UnknownComment,
    UnknownSuggestion,
    InvalidSubmitTarget,
    OriginAgentNotRunning,
    AmbiguousOriginSession,
    ReviewerAlreadyRunning,
    ReviewerBackendUnsupported,
    GitFailed,
    IoFailed,
    Internal,
}

impl ReviewErrorCode {
    pub const fn code_name(self) -> &'static str {
        match self {
            Self::InvalidStatus => "invalid_status",
            Self::InvalidLocation => "invalid_location",
            Self::UnknownComment => "unknown_comment",
            Self::UnknownSuggestion => "unknown_suggestion",
            Self::InvalidSubmitTarget => "invalid_submit_target",
            Self::OriginAgentNotRunning => "origin_agent_not_running",
            Self::AmbiguousOriginSession => "ambiguous_origin_session",
            Self::ReviewerAlreadyRunning => "reviewer_already_running",
            Self::ReviewerBackendUnsupported => "reviewer_backend_unsupported",
            Self::GitFailed => "git_failed",
            Self::IoFailed => "io_failed",
            Self::Internal => "internal",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReviewErrorContext {
    AddComment,
    UpdateComment { comment_id: ReviewCommentId },
    DeleteComment { comment_id: ReviewCommentId },
    AcceptSuggestion { suggestion_id: ReviewSuggestionId },
    RejectSuggestion { suggestion_id: ReviewSuggestionId },
    StartAiReview,
    Submit,
    ClearComments,
    Cancel,
}

impl ReviewErrorContext {
    pub const fn kind_name(&self) -> &'static str {
        match self {
            Self::AddComment => "add_comment",
            Self::UpdateComment { .. } => "update_comment",
            Self::DeleteComment { .. } => "delete_comment",
            Self::AcceptSuggestion { .. } => "accept_suggestion",
            Self::RejectSuggestion { .. } => "reject_suggestion",
            Self::StartAiReview => "start_ai_review",
            Self::Submit => "submit",
            Self::ClearComments => "clear_comments",
            Self::Cancel => "cancel",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewSummary {
    pub id: ReviewId,
    #[serde(default)]
    pub scope: ReviewSummaryScope,
    pub status: ReviewStatus,
    pub origin_session_id: SessionId,
    pub origin_agent_id: AgentId,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub user_comment_count: u32,
    pub pending_suggestion_count: u32,
    #[serde(default)]
    pub file_comment_counts: Vec<ReviewFileCommentCount>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReviewSummaryScope {
    #[default]
    Workspace,
    Root {
        root: ProjectRootPath,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewFileCommentCount {
    #[serde(default)]
    pub root: ProjectRootPath,
    pub relative_path: String,
    #[serde(default)]
    pub user_comment_count: u32,
    #[serde(default)]
    pub ai_comment_count: u32,
    #[serde(default)]
    pub pending_suggestion_count: u32,
}

impl ReviewFileCommentCount {
    pub const fn total_count(&self) -> u32 {
        self.user_comment_count
            .saturating_add(self.ai_comment_count)
            .saturating_add(self.pending_suggestion_count)
    }
}

/// Absolute host-native path. Server-owned semantics: interpretation is up to
/// the receiving host (POSIX vs Windows, home expansion, symlink policy).
/// Frontend never constructs, normalizes, or interprets the bytes.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HostAbsPath(pub String);

impl fmt::Display for HostAbsPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostPlatform {
    Macos,
    Linux,
    Windows,
    Other,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostBrowseStartPayload {
    /// `/browse/<uuid>` — client-allocated stream path on which the server
    /// will emit `HostBrowseOpened` / `HostBrowseEntries` / `HostBrowseError`.
    pub browse_stream: StreamPath,
    /// Server-owned intent for the initial directory to list.
    pub initial: HostBrowseInitial,
    pub include_hidden: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HostBrowseInitial {
    Home,
    Path { path: HostAbsPath },
    ProjectRoots { project_id: ProjectId },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostBrowseListPayload {
    pub path: HostAbsPath,
    pub include_hidden: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HostBrowseClosePayload {}

/// Seq 0 on `/browse/<uuid>`. Birth certificate of the browse stream — declares
/// the host's filesystem shape so the client never has to infer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostBrowseOpenedPayload {
    pub home: HostAbsPath,
    pub root: HostAbsPath,
    pub separator: char,
    pub platform: HostPlatform,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostBrowseEntriesPayload {
    pub path: HostAbsPath,
    pub parent: Option<HostAbsPath>,
    pub entries: Vec<HostBrowseEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostBrowseEntry {
    pub name: String,
    pub kind: ProjectFileKind,
    pub size: Option<u64>,
    pub mtime_ms: Option<u64>,
    pub is_hidden: bool,
    pub symlink_target: Option<HostAbsPath>,
    pub entry_error: Option<HostBrowseEntryError>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostBrowseEntryError {
    PermissionDenied,
    BrokenSymlink,
    StatFailed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostBrowseErrorPayload {
    pub path: HostAbsPath,
    pub code: HostBrowseErrorCode,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostBrowseErrorCode {
    NotFound,
    NotADirectory,
    PermissionDenied,
    SymlinkLoop,
    TooLarge,
    Internal,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TerminalId(pub String);

impl fmt::Display for TerminalId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TerminalLaunchTarget {
    HostDefault,
    Project {
        project_id: ProjectId,
        root: ProjectRootPath,
        relative_cwd: Option<String>,
    },
    Path {
        cwd: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalCreatePayload {
    pub target: TerminalLaunchTarget,
    pub cols: u16,
    pub rows: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalSendPayload {
    pub data: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalResizePayload {
    pub cols: u16,
    pub rows: u16,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TerminalClosePayload {}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewTerminalPayload {
    pub terminal_id: TerminalId,
    pub stream: StreamPath,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalStartPayload {
    pub project_id: Option<ProjectId>,
    pub root: Option<ProjectRootPath>,
    pub cwd: String,
    pub shell: String,
    pub cols: u16,
    pub rows: u16,
    pub created_at_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalOutputPayload {
    pub data: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalExitPayload {
    pub exit_code: Option<i32>,
    pub signal: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalErrorCode {
    NotRunning,
    IoFailed,
    Internal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalErrorPayload {
    pub code: TerminalErrorCode,
    pub message: String,
    pub fatal: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandErrorCode {
    InvalidInput,
    NotFound,
    Conflict,
    Internal,
    ProtocolViolation,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandErrorPayload {
    pub stream: StreamPath,
    pub request_kind: FrameKind,
    /// Present only for [`FrameKind::SetSetting`] errors. The target is
    /// intentionally value-free so command errors cannot expose submitted
    /// settings, credentials, paths, or projection tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setting_target: Option<HostSettingErrorTarget>,
    pub operation: String,
    pub code: CommandErrorCode,
    pub message: String,
    pub fatal: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentErrorPayload {
    pub agent_id: AgentId,
    pub code: AgentErrorCode,
    pub message: String,
    pub fatal: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OrchestrationId(pub String);

impl fmt::Display for OrchestrationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OrchestrationAgentType(pub String);

impl fmt::Display for OrchestrationAgentType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TycodeModel {
    #[serde(rename = "claude-fable")]
    ClaudeFable,
    #[serde(rename = "claude-opus")]
    ClaudeOpus,
    #[serde(rename = "claude-opus-fast")]
    ClaudeOpusFast,
    #[serde(rename = "claude-sonnet")]
    ClaudeSonnet,
    #[serde(rename = "claude-haiku")]
    ClaudeHaiku,
    #[serde(rename = "gpt")]
    Gpt,
    #[serde(rename = "gpt-pro")]
    GptPro,
    #[serde(rename = "gpt-mini")]
    GptMini,
    #[serde(rename = "gpt-codex")]
    GptCodex,
    #[serde(rename = "gpt-codex-max")]
    GptCodexMax,
    #[serde(rename = "gpt-oss-120b")]
    GptOss120b,
    #[serde(rename = "gpt-oss-120b-free")]
    GptOss120bFree,
    #[serde(rename = "gemini-flash")]
    GeminiFlash,
    #[serde(rename = "gemini-pro")]
    GeminiPro,
    #[serde(rename = "gemini-flash-lite")]
    GeminiFlashLite,
    #[serde(rename = "kimi-k2")]
    KimiK2,
    #[serde(rename = "kimi-k2-free")]
    KimiK2Free,
    #[serde(rename = "qwen-max")]
    QwenMax,
    #[serde(rename = "qwen-plus")]
    QwenPlus,
    #[serde(rename = "qwen-flash")]
    QwenFlash,
    #[serde(rename = "qwen-coder")]
    QwenCoder,
    #[serde(rename = "deepseek-pro")]
    DeepSeekPro,
    #[serde(rename = "deepseek-flash")]
    DeepSeekFlash,
    #[serde(rename = "deepseek-flash-free")]
    DeepSeekFlashFree,
    #[serde(rename = "glm")]
    Glm,
    #[serde(rename = "minimax-m2")]
    MinimaxM2,
    #[serde(rename = "grok")]
    Grok,
    #[serde(rename = "grok-build")]
    GrokBuild,
    #[serde(rename = "ring")]
    Ring,
    #[serde(rename = "step-flash")]
    StepFlash,
    #[serde(rename = "openrouter/auto")]
    OpenRouterAuto,
    #[serde(rename = "None")]
    None,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrchestrationEvent {
    pub agent_id: OrchestrationId,
    pub agent_type: OrchestrationAgentType,
    pub payload: OrchestrationPayload,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum OrchestrationPayload {
    AgentStarted {
        parent_agent_id: Option<OrchestrationId>,
        task_preview: String,
        origin: OrchestrationAgentOrigin,
        depth: usize,
        interactive: bool,
        model: Option<TycodeModel>,
    },
    AgentCompleted {
        status: OrchestrationOutcomeStatus,
        result: String,
    },
    PhaseChanged {
        phase: OrchestrationWorkflowPhase,
    },
    FanOutStarted {
        fanout_id: OrchestrationId,
        total: usize,
        concurrency: usize,
        workers: Vec<OrchestrationWorkerInfo>,
    },
    WorkerStarted {
        fanout_id: OrchestrationId,
        worker_id: OrchestrationId,
        label: String,
    },
    WorkerCompleted {
        fanout_id: OrchestrationId,
        worker_id: OrchestrationId,
        label: String,
        status: OrchestrationOutcomeStatus,
        summary: String,
    },
    FanOutCompleted {
        fanout_id: OrchestrationId,
        status: OrchestrationOutcomeStatus,
    },
    ConsensusRoundResolved {
        round: u32,
        verdicts: Vec<OrchestrationPanelVerdict>,
        eliminated: Option<OrchestrationCandidateInfo>,
        remaining: Vec<OrchestrationCandidateInfo>,
    },
    PlanSelected {
        candidate: Option<OrchestrationCandidateInfo>,
    },
    ReviewRoundResolved {
        round: u32,
        verdict: OrchestrationReviewVerdict,
        feedback: String,
    },
}

impl OrchestrationPayload {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::AgentStarted { .. } => "AgentStarted",
            Self::AgentCompleted { .. } => "AgentCompleted",
            Self::PhaseChanged { .. } => "PhaseChanged",
            Self::FanOutStarted { .. } => "FanOutStarted",
            Self::WorkerStarted { .. } => "WorkerStarted",
            Self::WorkerCompleted { .. } => "WorkerCompleted",
            Self::FanOutCompleted { .. } => "FanOutCompleted",
            Self::ConsensusRoundResolved { .. } => "ConsensusRoundResolved",
            Self::PlanSelected { .. } => "PlanSelected",
            Self::ReviewRoundResolved { .. } => "ReviewRoundResolved",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum OrchestrationAgentOrigin {
    Tool { tool_call_id: String },
    Workflow,
    Root,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrchestrationOutcomeStatus {
    Succeeded,
    Failed,
    /// The agent was discarded by an agent switch, conversation reset, or
    /// session change. Tycode turn cancellation is different:
    /// `ChatEvent::OperationCancelled` aborts in-flight fan-outs without
    /// terminal worker events, so consumers must close those locally.
    Aborted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrchestrationReviewVerdict {
    Approved,
    Rejected,
    RoundLimitReached,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrchestrationWorkerInfo {
    pub worker_id: OrchestrationId,
    pub label: String,
    pub agent_type: OrchestrationAgentType,
    pub model: Option<TycodeModel>,
    pub reviewed: bool,
    pub task_preview: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrchestrationCandidateInfo {
    pub label: String,
    pub author: Option<TycodeModel>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrchestrationPanelVerdict {
    pub judge: Option<TycodeModel>,
    pub position: OrchestrationPanelPosition,
    pub worst_vote: Option<OrchestrationCandidateInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum OrchestrationPanelPosition {
    Endorsed {
        candidate: OrchestrationCandidateInfo,
    },
    Revised,
    NoPosition,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum OrchestrationWorkflowPhase {
    Reviewing {
        round: u32,
    },
    Fixing {
        round: u32,
    },
    BuilderPlanning,
    BuilderImplementing,
    BuilderReviewing {
        round: u32,
    },
    BuilderFixing {
        round: u32,
    },
    SwarmPlanning,
    SwarmPlanFanOut {
        models: Vec<TycodeModel>,
    },
    SwarmConsensus {
        round: u32,
        candidates: Vec<OrchestrationCandidateInfo>,
    },
    SwarmImplementing {
        fixer_model: Option<TycodeModel>,
    },
    SwarmFanOut {
        model: Option<TycodeModel>,
    },
    SwarmIntegration {
        round: u32,
        models: Vec<TycodeModel>,
    },
    SwarmFixing {
        round: u32,
    },
}

/// Events a backend emits on a chat stream. Mirrors the Tycode
/// `ChatEvent` enum in `tycode-core/src/chat/events.rs`; any semantic
/// change must be made there first so every backend (Claude, Codex,
/// Antigravity, Kiro, Tycode) shares one contract.
///
/// ## Invariants backends MUST uphold
///
/// These are the rules the server-side `ProtocolValidator` enforces.
/// If a backend violates one the stream is terminated with a protocol
/// error — do not paper over it in the validator.
///
/// ### Stream pairing
/// Every `StreamStart` on a stream must be followed by exactly one
/// `StreamEnd` before the next `StreamStart` on the same stream. Backends must
/// reserve provider identities without publishing a stream until the response
/// has renderable text, reasoning, tools, or images; clients must not synthesize
/// fallback content, and backends must not infer evidence their provider schema
/// does not expose. `StreamDelta` /
/// `StreamReasoningDelta` are only valid between a `StreamStart` and
/// its matching `StreamEnd`.
///
/// ### Tool pairing
/// `ToolRequest` is only valid while an assistant turn is open (after a
/// `MessageAdded { Assistant }` or a `StreamStart`). Every emitted
/// `ToolRequest` must be answered by exactly one
/// `ToolExecutionCompleted` with the same `tool_call_id`.
///
/// ### Cancellation ordering
/// When a turn is cancelled the backend must, in this order:
///   1. If a stream is currently open, emit `StreamEnd` to close it. A reserved
///      identity that was never published has no stream to close.
///   2. Emit `ToolExecutionCompleted` for any outstanding
///      `ToolRequest`s the backend originated in this turn (mark them
///      unsuccessful / cancelled).
///   3. Emit exactly one `OperationCancelled`.
///   4. Emit `TypingStatusChanged(false)`.
///
/// This matches `tycode-core::chat::protocol::TurnProtocol::abort`.
/// Without step 1, the next turn's `StreamStart` violates the stream
/// pairing invariant above.
///
/// An identity-violation discard is the sole exception: because its active
/// stream is untrusted, the backend must not fabricate `StreamEnd` content.
/// It emits one visible error followed by `OperationCancelled` and
/// `TypingStatusChanged(false)`; validators discard the active stream at the
/// cancellation boundary and retain its id as terminal.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data")]
pub enum ChatEvent {
    MessageAdded(ChatMessage),
    MessageMetadataUpdated(MessageMetadataUpdateData),
    TypingStatusChanged(bool),
    StreamStart(StreamStartData),
    StreamDelta(StreamTextDeltaData),
    StreamReasoningDelta(StreamTextDeltaData),
    StreamEnd(StreamEndData),
    ToolRequest(ToolRequest),
    /// Live progress for a tool call. Zero or more may arrive for a
    /// `tool_call_id`, both before and *after* its
    /// `ToolExecutionCompleted` — background tasks (e.g. Claude Code
    /// workflows) outlive the tool call that started them, so progress
    /// keeps flowing after the tool result and across turn boundaries.
    /// Each event carries a full snapshot, never a delta: consumers keep
    /// only the latest per `tool_call_id`.
    ToolProgress(ToolProgressData),
    ToolExecutionCompleted(ToolExecutionCompletedData),
    TaskUpdate(TaskList),
    OperationCancelled(OperationCancelledData),
    RetryAttempt(RetryAttemptData),
    Orchestration(OrchestrationEvent),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MessageSender {
    User,
    System,
    Warning,
    Error,
    Assistant { agent: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    #[serde(default)]
    pub message_id: Option<ChatMessageId>,
    pub timestamp: u64,
    pub sender: MessageSender,
    pub content: String,
    pub reasoning: Option<ReasoningData>,
    pub tool_calls: Vec<ToolUseData>,
    pub model_info: Option<ModelInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_usage: Option<MessageTokenUsage>,
    pub context_breakdown: Option<ContextBreakdown>,
    pub images: Option<Vec<ImageData>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageMetadataUpdateData {
    pub message_id: ChatMessageId,
    pub model_info: Option<ModelInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_usage: Option<MessageTokenUsage>,
    pub context_breakdown: Option<ContextBreakdown>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReasoningData {
    pub text: String,
    pub tokens: Option<u64>,
    pub signature: Option<String>,
    pub blob: Option<Vec<u8>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolUseData {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub model: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub cached_prompt_tokens: Option<u64>,
    pub cache_creation_input_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ModelTurnId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ModelRequestId {
    pub turn_id: ModelTurnId,
    pub sequence: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRequestTokenUsage {
    pub request_id: ModelRequestId,
    pub request: TokenUsage,
    pub turn: TokenUsage,
    pub cumulative: TokenUsage,
    pub model_context_window: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageTokenUsage {
    pub request: TokenUsageScope,
    pub turn: TokenUsageScope,
    pub cumulative: TokenUsageScope,
}

impl MessageTokenUsage {
    pub fn unavailable(reason: TokenUsageUnavailableReason) -> Self {
        Self {
            request: TokenUsageScope::Unavailable { reason },
            turn: TokenUsageScope::Unavailable { reason },
            cumulative: TokenUsageScope::Unavailable { reason },
        }
    }

    pub fn request_known(usage: TokenUsage) -> Self {
        Self {
            request: TokenUsageScope::Known {
                usage: Box::new(usage),
            },
            turn: TokenUsageScope::Unavailable {
                reason: TokenUsageUnavailableReason::BackendDidNotReport,
            },
            cumulative: TokenUsageScope::Unavailable {
                reason: TokenUsageUnavailableReason::BackendDidNotReport,
            },
        }
    }

    pub fn request_and_turn_known(request: TokenUsage, turn: TokenUsage) -> Self {
        Self {
            request: TokenUsageScope::Known {
                usage: Box::new(request),
            },
            turn: TokenUsageScope::Known {
                usage: Box::new(turn),
            },
            cumulative: TokenUsageScope::Unavailable {
                reason: TokenUsageUnavailableReason::BackendDidNotReport,
            },
        }
    }

    pub fn with_cumulative(mut self, cumulative: TokenUsage) -> Self {
        self.cumulative = TokenUsageScope::Known {
            usage: Box::new(cumulative),
        };
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TokenUsageScope {
    Known { usage: Box<TokenUsage> },
    Unavailable { reason: TokenUsageUnavailableReason },
}

impl TokenUsageScope {
    pub fn known_usage(&self) -> Option<&TokenUsage> {
        match self {
            Self::Known { usage } => Some(usage),
            Self::Unavailable { .. } => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenUsageUnavailableReason {
    BackendDidNotReport,
    ProviderScopeAmbiguous,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextBreakdown {
    pub system_prompt_bytes: u64,
    pub tool_io_bytes: u64,
    pub conversation_history_bytes: u64,
    pub reasoning_bytes: u64,
    pub context_injection_bytes: u64,
    pub input_tokens: u64,
    pub context_window: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageData {
    pub media_type: String,
    pub data: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamStartData {
    /// Required for valid assistant stream frames. Kept optional in the wire
    /// type only so older persisted frames can still deserialize; validators
    /// reject a missing or empty value.
    pub message_id: Option<String>,
    pub agent: String,
    pub model: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamTextDeltaData {
    /// Required for valid assistant stream frames. It must equal the id from
    /// the matching `StreamStart`.
    pub message_id: Option<String>,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamEndData {
    /// `message.message_id` is required for valid assistant stream frames and
    /// must equal the id from the matching `StreamStart`.
    pub message: ChatMessage,
}

/// A value-free classification for an assistant stream identity violation.
///
/// The category is safe to surface across protocol boundaries: it carries no
/// provider payload, message text, or identifier value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamIdentityViolation {
    MissingMessageId,
    ForeignActiveMessageId,
    MismatchedEndMessageId,
    DuplicateTerminalMessageId,
    ConflictingDuplicateCompletion,
}

/// A server-authored contract for an assistant message whose provider response
/// does not expose a stable item identifier. Generation is explicit: callers
/// must persist and replay the same contract fields rather than infer an id
/// from text or stream timing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerGeneratedChatMessageIdentity {
    pub origin: ServerGeneratedChatMessageIdOrigin,
    pub stream_epoch: u64,
    pub item_ordinal: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServerGeneratedChatMessageIdOrigin {
    IdlessProviderResponseItem,
    IdlessReasoning,
    LegacyReplay,
}

impl ServerGeneratedChatMessageIdentity {
    /// Produces a deterministic, value-free message id from the persisted
    /// server contract.
    pub fn message_id(&self) -> ChatMessageId {
        let origin = match self.origin {
            ServerGeneratedChatMessageIdOrigin::IdlessProviderResponseItem => {
                "idless_provider_response_item"
            }
            ServerGeneratedChatMessageIdOrigin::IdlessReasoning => "idless_reasoning",
            ServerGeneratedChatMessageIdOrigin::LegacyReplay => "legacy_replay",
        };
        ChatMessageId(format!(
            "server-generated:{origin}:{}:{}",
            self.stream_epoch, self.item_ordinal
        ))
    }
}

impl StreamStartData {
    /// Converts the compatibility wire field into the required runtime
    /// assistant-stream identity.
    pub fn required_message_id(&self) -> Result<ChatMessageId, StreamIdentityViolation> {
        required_stream_message_id(&self.message_id)
    }
}

impl StreamTextDeltaData {
    /// Converts the compatibility wire field into the required runtime
    /// assistant-stream identity.
    pub fn required_message_id(&self) -> Result<ChatMessageId, StreamIdentityViolation> {
        required_stream_message_id(&self.message_id)
    }
}

impl StreamEndData {
    /// Returns the required immutable assistant-stream completion identity.
    pub fn required_message_id(&self) -> Result<ChatMessageId, StreamIdentityViolation> {
        let Some(message_id) = self
            .message
            .message_id
            .as_ref()
            .filter(|message_id| !message_id.0.trim().is_empty())
        else {
            return Err(StreamIdentityViolation::MissingMessageId);
        };
        Ok(message_id.clone())
    }
}

fn required_stream_message_id(
    message_id: &Option<String>,
) -> Result<ChatMessageId, StreamIdentityViolation> {
    let Some(message_id) = message_id
        .as_ref()
        .filter(|message_id| !message_id.trim().is_empty())
    else {
        return Err(StreamIdentityViolation::MissingMessageId);
    };
    Ok(ChatMessageId(message_id.clone()))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolRequest {
    pub tool_call_id: String,
    pub tool_name: String,
    pub tool_type: ToolRequestType,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum ToolRequestType {
    ModifyFile {
        file_path: String,
        before: String,
        after: String,
    },
    RunCommand {
        command: String,
        working_directory: String,
    },
    ReadFiles {
        file_paths: Vec<String>,
    },
    SearchTypes {
        language: String,
        workspace_root: String,
        type_name: String,
    },
    GetTypeDocs {
        language: String,
        workspace_root: String,
        type_path: String,
    },
    AskUserQuestion {
        questions: Vec<AskUserQuestion>,
    },
    ExitPlanMode {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        plan: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        plan_path: Option<String>,
    },
    /// A child-agent spawn, regardless of whether it originated from Tyde's
    /// agent-control MCP or a backend's native collaboration protocol.
    AgentSpawn {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prompt: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    GenerateImage {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prompt: Option<String>,
    },
    WebSearch {
        query: String,
    },
    ViewImage {
        path: String,
    },
    Sleep {
        duration_ms: u64,
    },
    /// `tyde_send_agent_message`: a follow-up message delivered to a direct
    /// child agent. The message is human-authored prose, so it is carried as
    /// canonical typed data rather than an opaque args blob — the UI renders it
    /// as Markdown instead of escaped JSON.
    TydeSendAgentMessage {
        agent_id: AgentId,
        message: String,
    },
    /// `tyde_await_agents`: the watched child agents. Everything else the await
    /// card shows (live name, status, usage) is resolved from server-owned agent
    /// state, so the id list is the whole request.
    TydeAwaitAgents {
        agent_ids: Vec<AgentId>,
    },
    Other {
        args: Value,
    },
}

/// One watched agent's terminal status in a `tyde_await_agents` completion.
/// Mirrors the MCP tool's own result shape — status only, never output text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TydeAgentWaitStatus {
    pub agent_id: AgentId,
    pub status: AgentControlStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskUserQuestion {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub question: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
    #[serde(default)]
    pub options: Vec<AskUserQuestionOption>,
    #[serde(default, rename = "multiSelect")]
    pub multi_select: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskUserQuestionOption {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolProgressData {
    pub tool_call_id: String,
    pub tool_name: String,
    pub update: ToolProgressUpdate,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolProgressUpdate {
    SubAgent(SubAgentProgress),
    Workflow(WorkflowRunState),
    AgentControl(AgentControlProgress),
    BackgroundTask(BackgroundTaskState),
    Other { payload: Value },
}

/// Live status of a backgrounded shell command (Claude Code `Bash` with
/// `run_in_background: true`), reduced server-side from the CLI's task
/// system frames (`task_started` / `task_updated` / `task_notification`).
/// Keyed to the launching tool call by `tool_call_id`, like a workflow.
/// The command string itself never appears in task frames — consumers
/// that want it join with the originating tool request by `tool_call_id`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackgroundTaskState {
    /// The CLI's task id (distinct from the tool_use id).
    pub task_id: String,
    /// Human description from the `task_started` frame — the model's
    /// `description` argument to the Bash tool.
    #[serde(default)]
    pub description: Option<String>,
    pub status: BackgroundTaskStatus,
    /// Completion summary from the `task_notification` frame, e.g.
    /// `Background command "…" completed (exit code 0)`. The exit code
    /// exists only inside this text; the CLI reports no structured field.
    #[serde(default)]
    pub summary: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackgroundTaskStatus {
    Running,
    Completed,
    /// Ended without completing — killed at session teardown or via
    /// TaskStop (`task_updated` patch status `killed`, notification
    /// status `stopped`).
    Stopped,
    Failed,
    #[serde(other)]
    Unknown,
}

/// Live status of a sub-agent spawned by a Task-style tool call,
/// emitted on the parent agent's stream so the Task tool card can show
/// activity and link to the sub-agent's own view.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubAgentProgress {
    pub agent_id: AgentId,
    pub agent_name: String,
    pub last_tool_name: Option<String>,
    pub tool_calls: u64,
    pub completed: bool,
}

/// Live Tyde agent-control MCP progress for tool cards that spawn or wait on
/// first-class Tyde agents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentControlProgress {
    pub progress_kind: AgentControlProgressKind,
    pub agents: Vec<AgentControlAgentRef>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentControlProgressKind {
    Spawn,
    Await,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentControlAgentRef {
    pub agent_id: AgentId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// Full snapshot of a Claude Code workflow run, reduced server-side
/// from the CLI's `task_progress` delta frames.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowRunState {
    pub workflow_name: String,
    #[serde(default)]
    pub description: Option<String>,
    /// The workflow script source, from the CLI's `task_started` frame.
    #[serde(default)]
    pub script: Option<String>,
    pub status: WorkflowRunStatus,
    /// Completion summary, from the CLI's `task_notification` frame.
    #[serde(default)]
    pub summary: Option<String>,
    pub total_tokens: u64,
    pub tool_uses: u64,
    pub duration_ms: u64,
    /// Ordered by `index` (the CLI's per-run agent counter).
    pub agents: Vec<WorkflowAgentState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowRunStatus {
    Running,
    Completed,
    Failed,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowAgentState {
    pub index: u64,
    pub label: String,
    #[serde(default)]
    pub phase_title: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    pub state: WorkflowAgentStatus,
    pub tokens: u64,
    pub tool_calls: u64,
    pub duration_ms: u64,
    pub attempt: u64,
    #[serde(default)]
    pub prompt_preview: Option<String>,
    #[serde(default)]
    pub result_preview: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowAgentStatus {
    Queued,
    Running,
    Done,
    Error,
    #[serde(other)]
    Unknown,
}

/// Identifies a canonical agent-control contract failure without exposing the
/// rejected request or result payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolExecutionNormalizationFailure {
    CanonicalRequest,
    CanonicalResult,
    CanonicalRequestAndResult,
}

impl ToolExecutionNormalizationFailure {
    pub fn combined_with(self, other: Self) -> Self {
        use ToolExecutionNormalizationFailure::{
            CanonicalRequest, CanonicalRequestAndResult, CanonicalResult,
        };

        match (self, other) {
            (CanonicalRequestAndResult, _) | (_, CanonicalRequestAndResult) => {
                CanonicalRequestAndResult
            }
            (CanonicalRequest, CanonicalResult) | (CanonicalResult, CanonicalRequest) => {
                CanonicalRequestAndResult
            }
            (failure, _) => failure,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolExecutionCompletedData {
    pub tool_call_id: String,
    pub tool_name: String,
    pub tool_result: ToolExecutionResult,
    pub success: bool,
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub normalization_failure: Option<ToolExecutionNormalizationFailure>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum ToolExecutionResult {
    ModifyFile {
        lines_added: u64,
        lines_removed: u64,
    },
    RunCommand {
        exit_code: i32,
        stdout: String,
        stderr: String,
    },
    ReadFiles {
        files: Vec<FileInfo>,
    },
    SearchTypes {
        types: Vec<String>,
    },
    GetTypeDocs {
        documentation: String,
    },
    Error {
        short_message: String,
        detailed_message: String,
    },
    /// Delivery acknowledgement for `tyde_send_agent_message`. The MCP tool
    /// returns `{"ok": true}` and nothing else, so there is no result body to
    /// render — the card's header status carries the whole outcome.
    TydeSendAgentMessage,
    /// `tyde_await_agents` verdict: which watched agents finished their turn and
    /// which were still thinking when the wait returned.
    TydeAwaitAgents {
        ready: Vec<TydeAgentWaitStatus>,
        still_thinking: Vec<TydeAgentWaitStatus>,
    },
    GenerateImage {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        revised_prompt: Option<String>,
        image_count: u64,
    },
    WebSearch,
    ViewImage,
    Sleep,
    Other {
        result: Value,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileInfo {
    pub path: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperationCancelledData {
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryAttemptData {
    pub attempt: u64,
    pub max_retries: u64,
    pub error: String,
    pub backoff_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: u64,
    pub description: String,
    pub status: TaskStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskList {
    pub title: String,
    pub tasks: Vec<Task>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeqMismatch {
    pub stream: StreamPath,
    pub kind: FrameKind,
    pub expected: u64,
    pub got: u64,
}

impl std::fmt::Display for SeqMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "sequence mismatch for stream {} kind {}: expected {}, got {}",
            self.stream, self.kind, self.expected, self.got
        )
    }
}

impl std::error::Error for SeqMismatch {}

#[derive(Debug, Default)]
pub struct SeqValidator {
    expected: HashMap<StreamPath, u64>,
}

impl SeqValidator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn validate(
        &mut self,
        stream: &StreamPath,
        seq: u64,
        kind: FrameKind,
    ) -> Result<(), SeqMismatch> {
        let expected = self.expected.get(stream).copied().unwrap_or(0);
        if seq != expected {
            return Err(SeqMismatch {
                stream: stream.clone(),
                kind,
                expected,
                got: seq,
            });
        }
        self.expected.insert(stream.clone(), expected + 1);
        Ok(())
    }
}

#[cfg(test)]
mod token_usage_serde_tests {
    use super::*;

    #[test]
    fn chat_message_token_usage_round_trips_all_scopes() {
        let request = TokenUsage {
            input_tokens: 1,
            output_tokens: 2,
            total_tokens: 3,
            cached_prompt_tokens: Some(4),
            cache_creation_input_tokens: Some(5),
            reasoning_tokens: Some(6),
        };
        let turn = TokenUsage {
            input_tokens: 10,
            output_tokens: 20,
            total_tokens: 30,
            cached_prompt_tokens: Some(40),
            cache_creation_input_tokens: Some(50),
            reasoning_tokens: Some(60),
        };
        let cumulative = TokenUsage {
            input_tokens: 100,
            output_tokens: 200,
            total_tokens: 300,
            cached_prompt_tokens: Some(400),
            cache_creation_input_tokens: Some(500),
            reasoning_tokens: Some(600),
        };

        let usage = MessageTokenUsage::request_and_turn_known(request.clone(), turn.clone())
            .with_cumulative(cumulative.clone());
        let json = serde_json::to_value(&usage).expect("serialize");
        assert_eq!(json["request"]["kind"], serde_json::json!("known"));
        assert_eq!(
            json["request"]["usage"]["total_tokens"],
            serde_json::json!(3)
        );
        assert_eq!(json["turn"]["usage"]["total_tokens"], serde_json::json!(30));
        assert_eq!(
            json["cumulative"]["usage"]["total_tokens"],
            serde_json::json!(300)
        );

        let round_trip: MessageTokenUsage =
            serde_json::from_value(json).expect("deserialize message token usage");
        assert_eq!(round_trip, usage);
        assert_eq!(round_trip.request.known_usage(), Some(&request));
        assert_eq!(round_trip.turn.known_usage(), Some(&turn));
        assert_eq!(round_trip.cumulative.known_usage(), Some(&cumulative));
    }

    #[test]
    fn token_usage_unavailable_reason_round_trips_provider_scope_ambiguous() {
        let usage = MessageTokenUsage {
            request: TokenUsageScope::Unavailable {
                reason: TokenUsageUnavailableReason::ProviderScopeAmbiguous,
            },
            turn: TokenUsageScope::Unavailable {
                reason: TokenUsageUnavailableReason::BackendDidNotReport,
            },
            cumulative: TokenUsageScope::Unavailable {
                reason: TokenUsageUnavailableReason::BackendDidNotReport,
            },
        };

        let json = serde_json::to_value(&usage).expect("serialize");
        assert_eq!(
            json["request"]["reason"],
            serde_json::json!("provider_scope_ambiguous")
        );
        assert_eq!(
            serde_json::from_value::<MessageTokenUsage>(json).expect("deserialize"),
            usage
        );
    }
}

#[cfg(test)]
mod command_error_serde_tests {
    use super::*;

    #[test]
    fn set_setting_error_targets_round_trip_without_echoing_setting_values() {
        let legacy = serde_json::json!({
            "stream": "/host/settings",
            "request_kind": "set_setting",
            "operation": "set_setting",
            "code": "conflict",
            "message": "setting changed",
            "fatal": false,
        });
        let legacy_payload: CommandErrorPayload =
            serde_json::from_value(legacy).expect("deserialize legacy command error");
        assert_eq!(legacy_payload.setting_target, None);
        let legacy_encoded =
            serde_json::to_value(&legacy_payload).expect("serialize legacy command error");
        assert!(legacy_encoded.get("setting_target").is_none());

        let setting = HostSettingValue::ResetTycodeManagedProjection {
            backend: BackendKind::Tycode,
            expected_projection_id: TycodeProjectionId("projection-01J-secret".to_owned()),
            expected_state_hash: TycodeProjectionStateHash("sha256:state-secret".to_owned()),
        };
        let payload = CommandErrorPayload {
            stream: StreamPath("/host/settings".to_owned()),
            request_kind: FrameKind::SetSetting,
            setting_target: Some(setting.error_target()),
            operation: "set_setting".to_owned(),
            code: CommandErrorCode::Conflict,
            message: "projection changed".to_owned(),
            fatal: false,
        };
        let encoded = serde_json::to_value(&payload).expect("serialize typed command error");
        assert_eq!(
            encoded["setting_target"],
            serde_json::json!("reset_tycode_managed_projection")
        );
        let encoded_text = encoded.to_string();
        assert!(!encoded_text.contains("projection-01J-secret"));
        assert!(!encoded_text.contains("sha256:state-secret"));

        let decoded: CommandErrorPayload =
            serde_json::from_value(encoded).expect("deserialize typed command error");
        assert_eq!(
            decoded.setting_target,
            Some(HostSettingErrorTarget::ResetTycodeManagedProjection)
        );
    }

    #[test]
    fn host_setting_error_targets_distinguish_native_legacy_and_host_saves() {
        assert_eq!(
            HostSettingValue::BackendNativeSettings {
                backend: BackendKind::Tycode,
                settings: serde_json::json!({"api_key": "native-secret"}),
            }
            .error_target(),
            HostSettingErrorTarget::BackendNativeSettings
        );
        assert_eq!(
            HostSettingValue::BackendConfig {
                backend: BackendKind::Tycode,
                values: BackendConfigValues::default(),
            }
            .error_target(),
            HostSettingErrorTarget::BackendConfig
        );
        assert_eq!(
            HostSettingValue::EnableMobileConnections { enabled: true }.error_target(),
            HostSettingErrorTarget::EnableMobileConnections
        );
    }

    #[test]
    fn tool_completion_normalization_failure_is_typed_and_backward_compatible() {
        let completion = ToolExecutionCompletedData {
            tool_call_id: "tool-normalization".to_owned(),
            tool_name: "tyde_send_agent_message".to_owned(),
            tool_result: ToolExecutionResult::TydeSendAgentMessage,
            success: false,
            error: Some("request could not be normalized".to_owned()),
            normalization_failure: Some(ToolExecutionNormalizationFailure::CanonicalRequest),
        };
        let encoded = serde_json::to_value(&completion).expect("serialize marked completion");
        assert_eq!(
            encoded["normalization_failure"],
            serde_json::json!("canonical_request")
        );
        let decoded: ToolExecutionCompletedData =
            serde_json::from_value(encoded).expect("deserialize marked completion");
        assert_eq!(
            decoded.normalization_failure,
            Some(ToolExecutionNormalizationFailure::CanonicalRequest)
        );

        let legacy = ToolExecutionCompletedData {
            tool_call_id: "tool-unrelated-error".to_owned(),
            tool_name: "run_command".to_owned(),
            tool_result: ToolExecutionResult::Error {
                short_message: "command failed".to_owned(),
                detailed_message: "exit status 1".to_owned(),
            },
            success: false,
            error: Some("exit status 1".to_owned()),
            normalization_failure: None,
        };
        let legacy_encoded =
            serde_json::to_value(&legacy).expect("serialize unrelated completion error");
        assert!(legacy_encoded.get("normalization_failure").is_none());
        let legacy_decoded: ToolExecutionCompletedData =
            serde_json::from_value(legacy_encoded).expect("deserialize legacy completion");
        assert_eq!(legacy_decoded.normalization_failure, None);
    }
}

#[cfg(test)]
mod search_serde_tests {
    use super::*;

    fn round_trip<T>(value: &T) -> T
    where
        T: Serialize + DeserializeOwned,
    {
        let json = serde_json::to_string(value).expect("serialize");
        serde_json::from_str(&json).expect("deserialize")
    }

    #[test]
    fn protocol_version_is_thirty_eight() {
        assert_eq!(PROTOCOL_VERSION, 38);
    }

    #[test]
    fn session_list_scope_round_trips_and_defaults_to_all() {
        let payload = ListSessionsPayload {
            scope: Some(SessionListScope::RootSessions),
            cursor: None,
            limit: Some(20),
        };
        let encoded = serde_json::to_value(&payload).expect("serialize ListSessionsPayload");
        assert_eq!(encoded["scope"], serde_json::json!("root_sessions"));
        let decoded: ListSessionsPayload =
            serde_json::from_value(encoded).expect("deserialize ListSessionsPayload");
        assert_eq!(decoded.scope, Some(SessionListScope::RootSessions));

        let legacy_page = serde_json::json!({
            "cursor": { "generation": 1, "offset": 0 },
            "limit": 64,
            "total_count": 0,
            "status": { "kind": "complete" },
        });
        let page: SessionListPageInfo =
            serde_json::from_value(legacy_page).expect("deserialize SessionListPageInfo");
        assert_eq!(page.scope, SessionListScope::AllSessions);
    }

    #[test]
    fn managed_broker_credentials_round_trip_without_debug_secret_leak() {
        let mut headers = BTreeMap::new();
        headers.insert(
            "x-amz-customauthorizer-name".to_owned(),
            "tycode-mobile-v1".to_owned(),
        );
        headers.insert("tycode-grant".to_owned(), "signed-grant-token".to_owned());
        let credentials = ManagedBrokerCredentials {
            grant_id: ManagedBrokerGrantId::new("grant_01J").expect("grant id"),
            client_id: ManagedBrokerClientId::new("tyde/prod/pair_01J/host/grant_01J")
                .expect("client id"),
            connect: ManagedBrokerConnectAuth {
                username: Some("tyde?x-amz-customauthorizer-name=tycode-mobile-v1".to_owned()),
                password: Some("signed-grant-token".to_owned()),
                websocket_url: Some(
                    BrokerUrl::new(
                        "wss://a1234567890-ats.iot.us-west-2.amazonaws.com/mqtt?x-amz-customauthorizer-name=tycode-mobile-v1&tycode-grant=signed-grant-token"
                    )
                    .expect("websocket url"),
                ),
                headers,
            },
            scope: ManagedBrokerCredentialScope {
                namespace: ManagedBrokerTopicNamespace::new("tyde/prod/pair_01J")
                    .expect("namespace"),
                role: ManagedBrokerRole::Host,
                publish: vec!["tyde/prod/pair_01J/rooms/+/host-to-client".to_owned()],
                subscribe: vec!["tyde/prod/pair_01J/rooms/+/client-to-host".to_owned()],
            },
            issued_at_ms: 1_760_000_000_000,
            expires_at_ms: 1_760_000_900_000,
        };

        let json = serde_json::to_value(&credentials).expect("serialize credentials");
        assert_eq!(json["grant_id"], "grant_01J");
        assert_eq!(json["scope"]["role"], "host");
        assert_eq!(
            json["connect"]["headers"]["x-amz-customauthorizer-name"],
            "tycode-mobile-v1"
        );
        assert_eq!(
            json["connect"]["headers"]["tycode-grant"],
            "signed-grant-token"
        );
        assert_eq!(
            json["connect"]["websocket_url"],
            "wss://a1234567890-ats.iot.us-west-2.amazonaws.com/mqtt?x-amz-customauthorizer-name=tycode-mobile-v1&tycode-grant=signed-grant-token"
        );
        assert_eq!(
            serde_json::from_value::<ManagedBrokerCredentials>(json)
                .expect("deserialize credentials"),
            credentials
        );

        let debug = format!("{:?}", credentials.connect);
        assert!(
            !debug.contains("signed-grant-token"),
            "debug output leaked managed broker grant: {debug}"
        );
        assert!(
            !debug.contains("a1234567890-ats.iot.us-west-2.amazonaws.com"),
            "debug output leaked managed broker websocket URL: {debug}"
        );
        assert!(
            !debug.contains("tycode-grant") && !debug.contains("x-amz-customauthorizer-name"),
            "debug output leaked managed broker grant/header details: {debug}"
        );
    }

    #[test]
    fn mobile_managed_access_states_are_protocol_typed() {
        let repair = MobileAccessStatePayload {
            broker_status: MobileBrokerStatus::RepairRequired {
                code: MobileAccessErrorCode::RepairRequired,
                message: "Legacy public broker pairing must be repaired".to_owned(),
            },
            pairing: MobilePairingState::RepairRequired {
                code: MobileAccessErrorCode::RepairRequired,
                message: "Legacy public broker pairing must be repaired".to_owned(),
            },
            paired_devices: vec![MobileDeviceSummary {
                device_id: MobileDeviceId("dev_01J".to_owned()),
                label: "Mike's iPhone".to_owned(),
                key_fingerprint: "sha256:abc".to_owned(),
                created_at_ms: 1,
                last_seen_at_ms: None,
                state: MobileDeviceState::RepairRequired,
            }],
        };

        let json = serde_json::to_value(&repair).expect("serialize mobile access state");
        assert_eq!(json["broker_status"]["kind"], "repair_required");
        assert_eq!(json["pairing"]["kind"], "repair_required");
        assert_eq!(json["paired_devices"][0]["state"], "repair_required");
        assert_eq!(
            serde_json::from_value::<MobileAccessStatePayload>(json)
                .expect("deserialize mobile access state"),
            repair
        );
    }

    #[test]
    fn mobile_service_auth_state_carries_paywall_outside_host_state() {
        let auth = MobileServiceAuthStatePayload {
            state: MobileServiceAuthState::PassRequired {
                message: "A Tyggs Pass is required".to_owned(),
                paywall_url: "https://tyggs.com/pass".to_owned(),
            },
        };

        let json = serde_json::to_value(&auth).expect("serialize mobile service auth");
        assert_eq!(json["state"]["kind"], "pass_required");
        assert_eq!(json["state"]["paywall_url"], "https://tyggs.com/pass");
        assert_eq!(
            serde_json::from_value::<MobileServiceAuthStatePayload>(json)
                .expect("deserialize mobile service auth"),
            auth
        );
    }

    #[test]
    fn managed_semantic_newtypes_reject_empty_deserialization() {
        assert!(serde_json::from_value::<ManagedBrokerRegion>(serde_json::json!("")).is_err());
        assert!(
            serde_json::from_value::<ManagedBrokerAuthorizerName>(serde_json::json!("")).is_err()
        );
        assert!(serde_json::from_value::<ManagedBrokerGrantId>(serde_json::json!("")).is_err());
        assert!(serde_json::from_value::<ManagedBrokerClientId>(serde_json::json!("")).is_err());
        assert!(
            serde_json::from_value::<ManagedBrokerTopicNamespace>(serde_json::json!("")).is_err()
        );
        assert!(serde_json::from_value::<MobilePairingOfferId>(serde_json::json!("")).is_err());
    }

    #[test]
    fn orchestration_event_round_trips_tycode_shape() {
        let event = ChatEvent::Orchestration(OrchestrationEvent {
            agent_id: OrchestrationId("boot-1-7".to_owned()),
            agent_type: OrchestrationAgentType("swarm".to_owned()),
            payload: OrchestrationPayload::WorkerCompleted {
                fanout_id: OrchestrationId("boot-1-8".to_owned()),
                worker_id: OrchestrationId("boot-1-9".to_owned()),
                label: "src/a.rs".to_owned(),
                status: OrchestrationOutcomeStatus::Succeeded,
                summary: "done".to_owned(),
            },
        });

        let json = serde_json::to_value(&event).expect("serialize");
        assert_eq!(json["kind"], "Orchestration");
        assert_eq!(json["data"]["agent_id"], "boot-1-7");
        assert_eq!(json["data"]["agent_type"], "swarm");
        assert_eq!(json["data"]["payload"]["kind"], "WorkerCompleted");
        assert_eq!(json["data"]["payload"]["status"], "Succeeded");

        let decoded: ChatEvent = serde_json::from_value(json).expect("deserialize");
        let ChatEvent::Orchestration(decoded) = decoded else {
            panic!("expected Orchestration event");
        };
        assert_eq!(decoded.agent_id.0, "boot-1-7");
        assert_eq!(decoded.agent_type.0, "swarm");
        assert!(matches!(
            decoded.payload,
            OrchestrationPayload::WorkerCompleted { .. }
        ));
    }

    #[test]
    fn background_agent_settings_defaults_are_safe() {
        let settings: HostSettings = serde_json::from_str("{}").expect("deserialize settings");
        assert!(settings.background_agent_features.auto_generate_agent_names);
        assert!(!settings.background_agent_features.agent_activity_summaries);
        assert!(settings.code_intel.language_server_paths.is_empty());
    }

    #[test]
    fn tycode_managed_projection_snapshot_round_trips_typed_provenance_and_advisories() {
        let provenance = BackendNativeSettingsProvenance::TycodeManagedProjection {
            managed_settings_path: HostAbsPath(
                "/Users/alice/.tycode/tyde-settings.toml".to_owned(),
            ),
            source_settings_path: HostAbsPath("/Users/alice/.tycode/settings.toml".to_owned()),
            source: TycodeProjectionSource::SharedSettings,
            tycode_version: Version {
                major: 0,
                minor: 10,
                patch: 0,
            },
            projection_id: TycodeProjectionId("projection-01J".to_owned()),
            created_at_ms: 1_760_000_000_000,
            source_digest: TycodeProjectionSourceDigest("sha256:abc123".to_owned()),
            original_unchanged: true,
            notice_pending: true,
        };
        let snapshot = BackendNativeSettingsSnapshot {
            backend_kind: BackendKind::Tycode,
            status: BackendConfigSnapshotStatus::Ready,
            settings: Some(serde_json::json!({"active_provider": "anthropic"})),
            groups: Vec::new(),
            message: None,
            provenance: Some(provenance.clone()),
            advisories: vec![
                BackendNativeSettingsAdvisory::NoProviderConfigured {
                    message: "Configure a provider to continue.".to_owned(),
                },
                BackendNativeSettingsAdvisory::UnsupportedActiveProvider {
                    provider: "legacy-provider".to_owned(),
                    message: "Choose a supported provider in Tyde's copy.".to_owned(),
                },
                BackendNativeSettingsAdvisory::BackendReported {
                    message: "Tycode reported a recoverable settings diagnostic.".to_owned(),
                },
            ],
            managed_projection_recovery: None,
        };

        let json = serde_json::to_value(&snapshot).expect("serialize native settings snapshot");
        assert_eq!(json["provenance"]["kind"], "tycode_managed_projection");
        assert_eq!(json["provenance"]["source"], "shared_settings");
        assert_eq!(json["provenance"]["projection_id"], "projection-01J");
        assert_eq!(json["provenance"]["source_digest"], "sha256:abc123");
        assert_eq!(json["advisories"][0]["kind"], "no_provider_configured");
        assert_eq!(json["advisories"][1]["kind"], "unsupported_active_provider");
        assert_eq!(json["advisories"][2]["kind"], "backend_reported");
        assert_eq!(round_trip(&snapshot), snapshot);
        assert_eq!(round_trip(&provenance), provenance);
        assert_eq!(
            round_trip(&TycodeProjectionSource::Defaults),
            TycodeProjectionSource::Defaults
        );
    }

    #[test]
    fn native_settings_snapshot_defaults_new_projection_fields_for_legacy_hosts() {
        let legacy = serde_json::json!({
            "backend_kind": "tycode",
            "status": "ready",
            "settings": {"active_provider": "anthropic"},
            "groups": [],
        });
        let snapshot: BackendNativeSettingsSnapshot =
            serde_json::from_value(legacy).expect("deserialize legacy native settings snapshot");
        assert_eq!(snapshot.provenance, None);
        assert!(snapshot.advisories.is_empty());
        assert_eq!(snapshot.managed_projection_recovery, None);

        let encoded = serde_json::to_value(snapshot).expect("serialize legacy-compatible snapshot");
        assert!(encoded.get("provenance").is_none());
        assert!(encoded.get("advisories").is_none());
        assert!(encoded.get("managed_projection_recovery").is_none());
    }

    #[test]
    fn tycode_projection_notice_acknowledgement_round_trips_with_typed_id() {
        let payload = SetSettingPayload {
            setting: HostSettingValue::AcknowledgeTycodeProjectionNotice {
                backend: BackendKind::Tycode,
                projection_id: TycodeProjectionId("projection-01J".to_owned()),
            },
        };
        let json = serde_json::to_value(&payload).expect("serialize notice acknowledgement");
        assert_eq!(
            json,
            serde_json::json!({
                "setting": {
                    "kind": "acknowledge_tycode_projection_notice",
                    "backend": "tycode",
                    "projection_id": "projection-01J",
                },
            })
        );
        assert_eq!(round_trip(&payload), payload);
    }

    #[test]
    fn managed_projection_recovery_and_reset_round_trip_with_exact_tokens() {
        let recovery = TycodeManagedProjectionRecoveryState::ManagedProjectionResetRequired {
            reason: "The managed settings and transaction journal do not form a proven pair."
                .to_owned(),
            expected_projection_id: TycodeProjectionId("projection-recovery-01J".to_owned()),
            expected_state_hash: TycodeProjectionStateHash("sha256:state-abc123".to_owned()),
        };
        let snapshot = BackendNativeSettingsSnapshot {
            backend_kind: BackendKind::Tycode,
            status: BackendConfigSnapshotStatus::Unavailable,
            settings: None,
            groups: Vec::new(),
            message: Some("Managed projection recovery is required.".to_owned()),
            provenance: None,
            advisories: Vec::new(),
            managed_projection_recovery: Some(recovery.clone()),
        };
        let snapshot_json =
            serde_json::to_value(&snapshot).expect("serialize recovery native settings snapshot");
        assert_eq!(
            snapshot_json["managed_projection_recovery"]["kind"],
            "managed_projection_reset_required"
        );
        assert_eq!(
            snapshot_json["managed_projection_recovery"]["expected_projection_id"],
            "projection-recovery-01J"
        );
        assert_eq!(
            snapshot_json["managed_projection_recovery"]["expected_state_hash"],
            "sha256:state-abc123"
        );
        assert_eq!(round_trip(&snapshot), snapshot);
        assert_eq!(round_trip(&recovery), recovery);

        let reset = SetSettingPayload {
            setting: HostSettingValue::ResetTycodeManagedProjection {
                backend: BackendKind::Tycode,
                expected_projection_id: TycodeProjectionId("projection-recovery-01J".to_owned()),
                expected_state_hash: TycodeProjectionStateHash("sha256:state-abc123".to_owned()),
            },
        };
        let reset_json = serde_json::to_value(&reset).expect("serialize managed projection reset");
        assert_eq!(
            reset_json,
            serde_json::json!({
                "setting": {
                    "kind": "reset_tycode_managed_projection",
                    "backend": "tycode",
                    "expected_projection_id": "projection-recovery-01J",
                    "expected_state_hash": "sha256:state-abc123",
                },
            })
        );
        assert_eq!(round_trip(&reset), reset);
    }

    #[test]
    fn activity_summary_state_round_trips() {
        let state = AgentActivitySummaryState::Fresh {
            summary: AgentActivitySummary {
                text: "Editing the backend scheduler.".to_owned(),
                generated_at_ms: 42,
                source_from_seq: Some(1),
                source_through_seq: Some(9),
            },
        };
        assert_eq!(round_trip(&state), state);

        let payload = AgentActivitySummaryPayload {
            agent_id: AgentId("agent-1".to_owned()),
            state: AgentActivitySummaryState::Stale {
                summary: AgentActivitySummary {
                    text: "Editing the backend scheduler.".to_owned(),
                    generated_at_ms: 42,
                    source_from_seq: Some(1),
                    source_through_seq: Some(9),
                },
                reason: AgentActivitySummaryStaleReason::NewActivity,
            },
        };
        assert_eq!(round_trip(&payload), payload);
    }

    #[test]
    fn activity_stats_payload_and_bootstrap_round_trip() {
        assert_eq!(
            FrameKind::AgentActivityStats.to_string(),
            "agent_activity_stats"
        );
        let stats = AgentActivityStats {
            last_output_line: Some("Done".to_owned()),
            tool_calls: 2,
            token_usage: TokenUsage {
                input_tokens: 10,
                output_tokens: 5,
                total_tokens: 15,
                cached_prompt_tokens: Some(3),
                cache_creation_input_tokens: Some(1),
                reasoning_tokens: Some(2),
            },
            source_through_seq: Some(42),
        };
        let payload = AgentActivityStatsPayload {
            agent_id: AgentId("agent-1".to_owned()),
            stats,
        };
        assert_eq!(round_trip(&payload), payload);

        let bootstrap = AgentBootstrapPayload {
            events: vec![AgentBootstrapEvent::AgentActivityStats(payload.clone())],
            latest_output: AgentControlOutput::Empty,
        };
        assert!(matches!(
            round_trip(&bootstrap).events.as_slice(),
            [AgentBootstrapEvent::AgentActivityStats(round_tripped)] if round_tripped == &payload
        ));
    }

    #[test]
    fn task_token_usage_payload_round_trips() {
        assert_eq!(FrameKind::TaskTokenUsage.to_string(), "task_token_usage");
        let usage = TaskTokenUsageAmount {
            total_tokens: 42,
            input_tokens: Some(30),
            output_tokens: Some(12),
            cached_prompt_tokens: Some(5),
            cache_creation_input_tokens: None,
            reasoning_tokens: Some(3),
        };
        let payload = TaskTokenUsagePayload {
            root_agent_id: AgentId("root".to_owned()),
            root_session_id: Some(SessionId("root-session".to_owned())),
            total: TaskTokenUsageAggregate {
                usage: usage.clone(),
                status: TaskTokenUsageStatus::Partial {
                    unavailable_count: 1,
                    reasons: vec![TaskTokenUsageUnavailableReason::BackendDidNotReport],
                },
            },
            self_usage: TaskTokenUsageScope::Known {
                usage: Box::new(usage.clone()),
            },
            descendant_usage: TaskTokenUsageAggregate {
                usage: TaskTokenUsageAmount::total_only(10),
                status: TaskTokenUsageStatus::Known,
            },
            descendant_count: 2,
            breakdown: vec![TaskTokenUsageEntry {
                agent_id: AgentId("root".to_owned()),
                session_id: Some(SessionId("root-session".to_owned())),
                parent_agent_id: None,
                parent_session_id: None,
                name: "Root".to_owned(),
                origin: AgentOrigin::User,
                backend_kind: BackendKind::Claude,
                model: Some("mock".to_owned()),
                depth: 0,
                tree_index: 0,
                usage: TaskTokenUsageScope::Unavailable {
                    reason: TaskTokenUsageUnavailableReason::ProviderScopeAmbiguous,
                },
            }],
        };

        assert_eq!(round_trip(&payload), payload);
        let partial_scope = TaskTokenUsageScope::Partial {
            usage: Box::new(usage.clone()),
            unavailable_count: 2,
            reasons: vec![
                TaskTokenUsageUnavailableReason::BackendDidNotReport,
                TaskTokenUsageUnavailableReason::ProviderScopeAmbiguous,
            ],
        };
        assert_eq!(round_trip(&partial_scope), partial_scope);
        assert_eq!(partial_scope.known_usage(), None);
        assert_eq!(partial_scope.reported_usage(), Some(&usage));
        let mut entry_without_agent_id =
            serde_json::to_value(&payload.breakdown[0]).expect("serialize entry");
        entry_without_agent_id
            .as_object_mut()
            .expect("entry object")
            .remove("agent_id");
        assert!(
            serde_json::from_value::<TaskTokenUsageEntry>(entry_without_agent_id).is_err(),
            "TaskTokenUsageEntry.agent_id is required for agent breakdown rows"
        );
        let bootstrap = HostBootstrapPayload {
            settings: HostSettings {
                enabled_backends: Vec::new(),
                default_backend: None,
                enable_mobile_connections: false,
                mobile_broker_url: None,
                tyde_debug_mcp_enabled: false,
                tyde_agent_control_mcp_enabled: true,
                complexity_tiers_enabled: false,
                backend_tier_configs: HashMap::new(),
                background_agent_features: BackgroundAgentFeaturesSettings::default(),
                supervisor: SupervisorSettings::default(),
                code_intel: CodeIntelSettings::default(),
                backend_config: HashMap::new(),
                launch_profiles: Vec::new(),
            },
            mobile_access: MobileAccessStatePayload {
                broker_status: MobileBrokerStatus::Disabled,
                pairing: MobilePairingState::Idle,
                paired_devices: Vec::new(),
            },
            backend_setup: BackendSetupPayload {
                backends: Vec::new(),
            },
            session_schemas: Vec::new(),
            backend_config_schemas: Vec::new(),
            backend_config_snapshots: Vec::new(),
            launch_profile_catalog: LaunchProfileCatalog::default(),
            sessions: Vec::new(),
            session_list: Default::default(),
            projects: Vec::new(),
            mcp_servers: Vec::new(),
            skills: Vec::new(),
            steering: Vec::new(),
            custom_agents: Vec::new(),
            team_preset_catalog: TeamPresetCatalog {
                role_presets: Vec::new(),
                personality_traits: Vec::new(),
                personality_presets: Vec::new(),
                team_templates: Vec::new(),
            },
            team_drafts: Vec::new(),
            teams: Vec::new(),
            team_members: Vec::new(),
            team_member_bindings: Vec::new(),
            agents: Vec::new(),
            task_token_usages: vec![payload.clone()],
            workflow_summaries: Vec::new(),
            workflow_diagnostics: Vec::new(),
            workflow_runs: Vec::new(),
            workflow_locations: Vec::new(),
            agents_view_preferences: None,
        };
        assert_eq!(round_trip(&bootstrap).task_token_usages, vec![payload]);
    }

    #[test]
    fn sidebar_preferences_default_and_update_round_trip() {
        let snapshot: AgentsViewPreferencesSnapshot =
            serde_json::from_str(r#"{"preferences":{"filters":{}}}"#)
                .expect("deserialize snapshot");
        assert_eq!(snapshot.sidebar, AgentsSidebarPreferences::default());

        let update = AgentsViewPreferencesUpdate::SetSidebarPreferences {
            sidebar: AgentsSidebarPreferences {
                hide_inactive: true,
                hide_sub_agents: true,
                project_visibility: AgentsSidebarProjectVisibility::CurrentProjectOnly,
            },
        };
        let json = serde_json::to_value(&update).expect("serialize update");
        assert_eq!(json["kind"], "set_sidebar_preferences");
        assert_eq!(
            json["sidebar"]["project_visibility"],
            "current_project_only"
        );
        assert_eq!(round_trip(&update), update);
    }

    #[test]
    fn search_frame_kinds_display_snake_case() {
        assert_eq!(FrameKind::SetAgentGroups.to_string(), "set_agent_groups");
        assert_eq!(FrameKind::ProjectSearch.to_string(), "project_search");
        assert_eq!(
            FrameKind::ProjectSearchCancel.to_string(),
            "project_search_cancel"
        );
        assert_eq!(FrameKind::ProjectAccessed.to_string(), "project_accessed");
        assert_eq!(
            FrameKind::ProjectSearchResults.to_string(),
            "project_search_results"
        );
        assert_eq!(
            FrameKind::ProjectSearchComplete.to_string(),
            "project_search_complete"
        );
    }

    #[test]
    fn project_search_payload_round_trip() {
        let payload = ProjectSearchPayload {
            search_id: 7,
            query: "needle".to_owned(),
            case_sensitive: true,
            whole_word: true,
            use_regex: false,
            include_ignored: true,
            roots: vec![
                ProjectRootPath("/a".to_owned()),
                ProjectRootPath("/b".to_owned()),
            ],
            path_prefix: Some("src/".to_owned()),
            max_results: Some(500),
        };
        assert_eq!(round_trip(&payload), payload);
    }

    #[test]
    fn project_search_payload_defaults_deserialize() {
        // Minimal payload: only the required fields. Booleans/roots default.
        let payload: ProjectSearchPayload =
            serde_json::from_str(r#"{"search_id":1,"query":"x"}"#).expect("deserialize");
        assert_eq!(payload.search_id, 1);
        assert_eq!(payload.query, "x");
        assert!(!payload.case_sensitive);
        assert!(!payload.whole_word);
        assert!(!payload.use_regex);
        assert!(!payload.include_ignored);
        assert!(payload.roots.is_empty());
        assert_eq!(payload.path_prefix, None);
        assert_eq!(payload.max_results, None);
    }

    #[test]
    fn project_search_results_payload_round_trip() {
        let payload = ProjectSearchResultsPayload {
            search_id: 3,
            file: ProjectSearchFileResult {
                path: ProjectPath {
                    root: ProjectRootPath("/repo".to_owned()),
                    relative_path: "src/main.rs".to_owned(),
                },
                matches: vec![
                    ProjectSearchMatch {
                        line_number: 12,
                        line_text: "let café = needle;".to_owned(),
                        ranges: vec![(11, 17)],
                    },
                    ProjectSearchMatch {
                        line_number: 40,
                        line_text: "another needle here".to_owned(),
                        ranges: vec![(8, 14)],
                    },
                ],
                truncated: true,
            },
        };
        assert_eq!(round_trip(&payload), payload);
    }

    #[test]
    fn project_search_complete_round_trip() {
        let payload = ProjectSearchCompletePayload {
            search_id: 9,
            total_files: 4,
            total_matches: 17,
            truncated: false,
            cancelled: true,
            error: Some("boom".to_owned()),
        };
        assert_eq!(round_trip(&payload), payload);
    }

    #[test]
    fn project_search_cancel_round_trip() {
        let payload = ProjectSearchCancelPayload { search_id: 42 };
        assert_eq!(round_trip(&payload), payload);
    }

    #[test]
    fn project_accessed_payload_round_trip_empty_payload() {
        let payload = ProjectAccessedPayload {};
        let json = serde_json::to_value(&payload).expect("serialize");
        assert_eq!(json, serde_json::json!({}));
        assert_eq!(round_trip(&payload), payload);
        let kind_json = serde_json::to_string(&FrameKind::ProjectAccessed).expect("serialize");
        assert_eq!(kind_json, "\"project_accessed\"");
        assert_eq!(
            serde_json::from_str::<FrameKind>(&kind_json).expect("deserialize"),
            FrameKind::ProjectAccessed
        );
    }

    #[test]
    fn project_file_contents_carries_version() {
        let payload = ProjectFileContentsPayload {
            path: ProjectPath {
                root: ProjectRootPath("/repo".to_owned()),
                relative_path: "src/main.rs".to_owned(),
            },
            version: ProjectFileVersion(7),
            contents: Some("fn main() {}".to_owned()),
            is_binary: false,
            missing: false,
        };
        let json = serde_json::to_value(&payload).expect("serialize");
        assert_eq!(json["version"], serde_json::json!(7));
    }
}

#[cfg(test)]
mod code_intel_serde_tests {
    use super::*;

    fn round_trip<T>(value: &T) -> T
    where
        T: Serialize + DeserializeOwned,
    {
        let json = serde_json::to_string(value).expect("serialize");
        serde_json::from_str(&json).expect("deserialize")
    }

    fn sample_path() -> ProjectPath {
        ProjectPath {
            root: ProjectRootPath("/repo".to_owned()),
            relative_path: "src/lib.rs".to_owned(),
        }
    }

    fn sample_location() -> CodeIntelLocation {
        CodeIntelLocation {
            path: sample_path(),
            range: ByteRange { start: 4, end: 9 },
        }
    }

    #[test]
    fn code_intel_frame_kinds_display_snake_case() {
        assert_eq!(
            FrameKind::CodeIntelSubscribeFile.to_string(),
            "code_intel_subscribe_file"
        );
        assert_eq!(
            FrameKind::CodeIntelUnsubscribeFile.to_string(),
            "code_intel_unsubscribe_file"
        );
        assert_eq!(
            FrameKind::CodeIntelSetVisibleRange.to_string(),
            "code_intel_set_visible_range"
        );
        assert_eq!(FrameKind::CodeIntelHover.to_string(), "code_intel_hover");
        assert_eq!(
            FrameKind::CodeIntelNavigate.to_string(),
            "code_intel_navigate"
        );
        assert_eq!(
            FrameKind::CodeIntelFindReferences.to_string(),
            "code_intel_find_references"
        );
        assert_eq!(
            FrameKind::CodeIntelCancelReferences.to_string(),
            "code_intel_cancel_references"
        );
        assert_eq!(
            FrameKind::CodeIntelOverview.to_string(),
            "code_intel_overview"
        );
        assert_eq!(FrameKind::CodeIntelStatus.to_string(), "code_intel_status");
        assert_eq!(
            FrameKind::CodeIntelFileModel.to_string(),
            "code_intel_file_model"
        );
        assert_eq!(
            FrameKind::CodeIntelDiagnostics.to_string(),
            "code_intel_diagnostics"
        );
        assert_eq!(
            FrameKind::CodeIntelHoverResult.to_string(),
            "code_intel_hover_result"
        );
        assert_eq!(
            FrameKind::CodeIntelNavigateResult.to_string(),
            "code_intel_navigate_result"
        );
        assert_eq!(
            FrameKind::CodeIntelReferencesResults.to_string(),
            "code_intel_references_results"
        );
        assert_eq!(
            FrameKind::CodeIntelReferencesComplete.to_string(),
            "code_intel_references_complete"
        );
        assert_eq!(FrameKind::CodeIntelError.to_string(), "code_intel_error");
    }

    #[test]
    fn subscribe_unsubscribe_round_trip() {
        let subscribe = CodeIntelSubscribeFilePayload {
            path: sample_path(),
        };
        assert_eq!(round_trip(&subscribe), subscribe);
        let unsubscribe = CodeIntelUnsubscribeFilePayload {
            path: sample_path(),
        };
        assert_eq!(round_trip(&unsubscribe), unsubscribe);
    }

    #[test]
    fn set_visible_range_round_trip() {
        let payload = CodeIntelSetVisibleRangePayload {
            path: sample_path(),
            version: ProjectFileVersion(3),
            range: ByteRange { start: 0, end: 120 },
        };
        assert_eq!(round_trip(&payload), payload);
    }

    #[test]
    fn hover_and_navigate_round_trip() {
        let hover = CodeIntelHoverPayload {
            hover_id: 1,
            path: sample_path(),
            version: ProjectFileVersion(2),
            offset: 42,
        };
        assert_eq!(round_trip(&hover), hover);
        let navigate = CodeIntelNavigatePayload {
            navigate_id: 9,
            path: sample_path(),
            version: ProjectFileVersion(2),
            offset: 42,
        };
        assert_eq!(round_trip(&navigate), navigate);
    }

    #[test]
    fn find_and_cancel_references_round_trip() {
        let find = CodeIntelFindReferencesPayload {
            references_id: 5,
            path: sample_path(),
            version: ProjectFileVersion(4),
            offset: 17,
            include_declaration: true,
        };
        assert_eq!(round_trip(&find), find);
        let cancel = CodeIntelCancelReferencesPayload { references_id: 5 };
        assert_eq!(round_trip(&cancel), cancel);
    }

    #[test]
    fn overview_payload_round_trips_provider_state() {
        let payload = CodeIntelOverviewPayload {
            roots: vec![
                CodeIntelRootOverview {
                    root: ProjectRootPath("/repo-a".to_owned()),
                    providers: vec![CodeIntelProviderStatus {
                        provider: CodeIntelProviderId("rust-analyzer".to_owned()),
                        language: CodeIntelLanguageId("rust".to_owned()),
                        state: CodeIntelState::Indexing,
                        resource_mode: CodeIntelResourceMode::Full,
                        work_done: Some(40),
                        total_work: Some(100),
                        message: Some("indexing".to_owned()),
                        error_count: 3,
                        warning_count: 1,
                    }],
                },
                CodeIntelRootOverview {
                    root: ProjectRootPath("/repo-b".to_owned()),
                    providers: Vec::new(),
                },
            ],
            summary: CodeIntelOverviewSummary {
                headline: CodeIntelOverviewHeadline::Indexing,
                ready: 0,
                indexing: 1,
                starting: 0,
                unavailable: 0,
                failed: 0,
                message: Some("Indexing code intelligence".to_owned()),
                error_count: 3,
                warning_count: 1,
            },
        };
        assert_eq!(round_trip(&payload), payload);
    }

    #[test]
    fn overview_headline_round_trips_not_started() {
        let payload = CodeIntelOverviewPayload {
            roots: vec![CodeIntelRootOverview {
                root: ProjectRootPath("/repo".to_owned()),
                providers: Vec::new(),
            }],
            summary: CodeIntelOverviewSummary {
                headline: CodeIntelOverviewHeadline::NotStarted,
                ready: 0,
                indexing: 0,
                starting: 0,
                unavailable: 0,
                failed: 0,
                message: Some("No language server running — open a file to index".to_owned()),
                error_count: 0,
                warning_count: 0,
            },
        };

        let json = serde_json::to_value(&payload).expect("serialize");
        assert_eq!(json["summary"]["headline"], "not_started");
        assert_eq!(round_trip(&payload), payload);
    }

    #[test]
    fn status_payload_round_trips_every_scope_and_state() {
        let scopes = [
            CodeIntelStatusScope::Project,
            CodeIntelStatusScope::Provider {
                root: ProjectRootPath("/repo".to_owned()),
            },
            CodeIntelStatusScope::File {
                path: sample_path(),
                version: ProjectFileVersion(8),
            },
        ];
        let states = [
            CodeIntelState::Unsupported,
            CodeIntelState::Unavailable,
            CodeIntelState::Starting,
            CodeIntelState::Indexing,
            CodeIntelState::Ready,
            CodeIntelState::Failed,
        ];
        let modes = [
            CodeIntelResourceMode::Full,
            CodeIntelResourceMode::Limited,
            CodeIntelResourceMode::Unavailable,
        ];
        for scope in &scopes {
            for state in &states {
                for mode in &modes {
                    let payload = CodeIntelStatusPayload {
                        scope: scope.clone(),
                        state: *state,
                        resource_mode: *mode,
                        work_done: Some(3),
                        total_work: Some(10),
                        message: Some("indexing".to_owned()),
                    };
                    assert_eq!(round_trip(&payload), payload);
                }
            }
        }
    }

    #[test]
    fn file_model_round_trip_all_variants() {
        for model_range in [
            CodeIntelModelRange::FullFile,
            CodeIntelModelRange::ByteRange {
                range: ByteRange { start: 1, end: 2 },
            },
        ] {
            for completeness in [
                CodeIntelCompleteness::Complete,
                CodeIntelCompleteness::Partial,
            ] {
                let payload = CodeIntelFileModelPayload {
                    path: sample_path(),
                    version: ProjectFileVersion(6),
                    provider: CodeIntelProviderId("rust-analyzer".to_owned()),
                    language: CodeIntelLanguageId("rust".to_owned()),
                    model_range: model_range.clone(),
                    completeness,
                    occurrences: vec![
                        CodeIntelOccurrence {
                            range: ByteRange { start: 4, end: 9 },
                            role: CodeIntelRole::Definition,
                            display: "foo".to_owned(),
                            definition: vec![sample_location()],
                        },
                        CodeIntelOccurrence {
                            range: ByteRange { start: 20, end: 23 },
                            role: CodeIntelRole::Reference,
                            display: "bar".to_owned(),
                            definition: vec![],
                        },
                    ],
                };
                assert_eq!(round_trip(&payload), payload);
            }
        }
    }

    #[test]
    fn diagnostics_round_trip_all_severities() {
        for severity in [
            CodeIntelSeverity::Error,
            CodeIntelSeverity::Warning,
            CodeIntelSeverity::Information,
            CodeIntelSeverity::Hint,
        ] {
            let payload = CodeIntelDiagnosticsPayload {
                path: sample_path(),
                version: ProjectFileVersion(2),
                diagnostics: vec![CodeIntelDiagnostic {
                    range: ByteRange { start: 0, end: 5 },
                    severity,
                    message: "mismatched types".to_owned(),
                    source: Some("rustc".to_owned()),
                }],
            };
            assert_eq!(round_trip(&payload), payload);
        }
    }

    #[test]
    fn navigate_and_hover_results_round_trip() {
        let navigate = CodeIntelNavigateResultPayload {
            navigate_id: 9,
            path: sample_path(),
            version: ProjectFileVersion(2),
            targets: vec![sample_location()],
            external_targets: 1,
        };
        assert_eq!(round_trip(&navigate), navigate);
        let hover = CodeIntelHoverResultPayload {
            hover_id: 1,
            path: sample_path(),
            version: ProjectFileVersion(2),
            contents: Some("`fn foo()`".to_owned()),
            range: Some(ByteRange { start: 4, end: 9 }),
        };
        assert_eq!(round_trip(&hover), hover);
    }

    #[test]
    fn references_results_and_complete_round_trip() {
        let results = CodeIntelReferencesResultsPayload {
            references_id: 5,
            file: CodeIntelReferencesFileResult {
                path: sample_path(),
                lines: vec![CodeIntelReferenceLine {
                    line_number: 12,
                    line_text: "    foo();".to_owned(),
                    ranges: vec![ByteRange { start: 4, end: 7 }],
                }],
                truncated: false,
            },
        };
        assert_eq!(round_trip(&results), results);
        let complete = CodeIntelReferencesCompletePayload {
            references_id: 5,
            total_files: 2,
            total_references: 7,
            truncated: false,
            cancelled: false,
            error: None,
        };
        assert_eq!(round_trip(&complete), complete);
    }

    #[test]
    fn error_round_trip_all_codes_and_contexts() {
        let codes = [
            CodeIntelErrorCode::ProviderUnavailable,
            CodeIntelErrorCode::ProviderCrashed,
            CodeIntelErrorCode::UnsupportedLanguage,
            CodeIntelErrorCode::StaleVersion,
            CodeIntelErrorCode::Timeout,
            CodeIntelErrorCode::ProtocolError,
            CodeIntelErrorCode::Internal,
        ];
        let contexts = [
            CodeIntelErrorContext::Subscribe {
                path: sample_path(),
            },
            CodeIntelErrorContext::Hover {
                hover_id: 1,
                path: sample_path(),
            },
            CodeIntelErrorContext::Navigate {
                navigate_id: 2,
                path: sample_path(),
            },
            CodeIntelErrorContext::FindReferences {
                references_id: 3,
                path: sample_path(),
            },
            CodeIntelErrorContext::Provider {
                language: CodeIntelLanguageId("rust".to_owned()),
            },
        ];
        for code in &codes {
            for context in &contexts {
                let payload = CodeIntelErrorPayload {
                    code: *code,
                    message: "boom".to_owned(),
                    hint: Some("rustup component add rust-analyzer".to_owned()),
                    exit_status: Some("exit status: 1".to_owned()),
                    stderr: Some("language server stderr".to_owned()),
                    context: context.clone(),
                    fatal: true,
                };
                assert_eq!(round_trip(&payload), payload);
            }
        }
    }

    #[test]
    fn occurrence_definition_defaults_to_empty() {
        let occurrence: CodeIntelOccurrence = serde_json::from_str(
            r#"{"range":{"start":0,"end":3},"role":"reference","display":"x"}"#,
        )
        .expect("deserialize");
        assert!(occurrence.definition.is_empty());
    }
}

#[cfg(test)]
mod tool_progress_serde_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn agent_control_progress_round_trip() {
        let payload = ToolProgressData {
            tool_call_id: "toolu_await".to_owned(),
            tool_name: "tyde_await_agents".to_owned(),
            update: ToolProgressUpdate::AgentControl(AgentControlProgress {
                progress_kind: AgentControlProgressKind::Await,
                agents: vec![AgentControlAgentRef {
                    agent_id: AgentId("agent-123".to_owned()),
                    name: Some("Worker".to_owned()),
                }],
            }),
        };

        let encoded = serde_json::to_value(&payload).expect("serialize");
        assert_eq!(
            encoded,
            json!({
                "tool_call_id": "toolu_await",
                "tool_name": "tyde_await_agents",
                "update": {
                    "kind": "agent_control",
                    "progress_kind": "await",
                    "agents": [{
                        "agent_id": "agent-123",
                        "name": "Worker"
                    }]
                }
            })
        );

        let decoded: ToolProgressData = serde_json::from_value(encoded).expect("deserialize");
        let ToolProgressUpdate::AgentControl(progress) = decoded.update else {
            panic!("expected AgentControl progress");
        };
        assert_eq!(progress.progress_kind, AgentControlProgressKind::Await);
        assert_eq!(progress.agents.len(), 1);
        assert_eq!(progress.agents[0].agent_id, AgentId("agent-123".to_owned()));
        assert_eq!(progress.agents[0].name.as_deref(), Some("Worker"));
    }
}

/// Wire contract for the typed Tyde orchestration tool calls. These variants are
/// what let the UI render a sent message as Markdown and an await verdict as a
/// status list instead of dumping the MCP envelope as raw JSON, so their shape is
/// pinned here rather than left to whichever backend normalizes them.
#[cfg(test)]
mod tyde_orchestration_tool_serde_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn send_agent_message_request_round_trips() {
        let request = ToolRequest {
            tool_call_id: "toolu_send".to_owned(),
            tool_name: "tyde_send_agent_message".to_owned(),
            tool_type: ToolRequestType::TydeSendAgentMessage {
                agent_id: AgentId("agent-123".to_owned()),
                message: "**Fix the rerun**\n\n- start with `mock.rs`".to_owned(),
            },
        };

        let encoded = serde_json::to_value(&request).expect("serialize");
        assert_eq!(
            encoded,
            json!({
                "tool_call_id": "toolu_send",
                "tool_name": "tyde_send_agent_message",
                "tool_type": {
                    "kind": "TydeSendAgentMessage",
                    "agent_id": "agent-123",
                    "message": "**Fix the rerun**\n\n- start with `mock.rs`"
                }
            })
        );

        let decoded: ToolRequest = serde_json::from_value(encoded).expect("deserialize");
        let ToolRequestType::TydeSendAgentMessage { agent_id, message } = decoded.tool_type else {
            panic!("expected TydeSendAgentMessage request");
        };
        assert_eq!(agent_id, AgentId("agent-123".to_owned()));
        // The Markdown source survives verbatim — newlines stay newlines, not
        // the escaped `\n` the raw-JSON panel used to show.
        assert_eq!(message, "**Fix the rerun**\n\n- start with `mock.rs`");
    }

    /// The send tool's real result is `{"ok": true}` — a pure ack. The typed
    /// completion is a unit variant so there is nothing to render but status.
    #[test]
    fn send_agent_message_result_round_trips_as_bare_ack() {
        let result = ToolExecutionResult::TydeSendAgentMessage;
        let encoded = serde_json::to_value(&result).expect("serialize");
        assert_eq!(encoded, json!({ "kind": "TydeSendAgentMessage" }));

        let decoded: ToolExecutionResult = serde_json::from_value(encoded).expect("deserialize");
        assert_eq!(decoded, ToolExecutionResult::TydeSendAgentMessage);
    }

    #[test]
    fn await_agents_request_round_trips() {
        let request = ToolRequest {
            tool_call_id: "toolu_await".to_owned(),
            tool_name: "tyde_await_agents".to_owned(),
            tool_type: ToolRequestType::TydeAwaitAgents {
                agent_ids: vec![AgentId("agent-1".to_owned()), AgentId("agent-2".to_owned())],
            },
        };

        let encoded = serde_json::to_value(&request).expect("serialize");
        assert_eq!(
            encoded,
            json!({
                "tool_call_id": "toolu_await",
                "tool_name": "tyde_await_agents",
                "tool_type": {
                    "kind": "TydeAwaitAgents",
                    "agent_ids": ["agent-1", "agent-2"]
                }
            })
        );

        let decoded: ToolRequest = serde_json::from_value(encoded).expect("deserialize");
        let ToolRequestType::TydeAwaitAgents { agent_ids } = decoded.tool_type else {
            panic!("expected TydeAwaitAgents request");
        };
        assert_eq!(
            agent_ids,
            vec![AgentId("agent-1".to_owned()), AgentId("agent-2".to_owned())]
        );
    }

    /// Mirrors the MCP tool's `AwaitAgentsResult` exactly: `ready` /
    /// `still_thinking`, each carrying `{agent_id, status}` and nothing else.
    #[test]
    fn await_agents_result_round_trips() {
        let result = ToolExecutionResult::TydeAwaitAgents {
            ready: vec![TydeAgentWaitStatus {
                agent_id: AgentId("agent-1".to_owned()),
                status: AgentControlStatus::Idle,
            }],
            still_thinking: vec![TydeAgentWaitStatus {
                agent_id: AgentId("agent-2".to_owned()),
                status: AgentControlStatus::Thinking,
            }],
        };

        let encoded = serde_json::to_value(&result).expect("serialize");
        assert_eq!(
            encoded,
            json!({
                "kind": "TydeAwaitAgents",
                "ready": [{ "agent_id": "agent-1", "status": "idle" }],
                "still_thinking": [{ "agent_id": "agent-2", "status": "thinking" }]
            })
        );

        let decoded: ToolExecutionResult = serde_json::from_value(encoded).expect("deserialize");
        assert_eq!(decoded, result);
    }

    /// A failed watched agent surfaces as `failed` in `ready` — the wait is over
    /// for it. The status enum, not a string, is what crosses the wire.
    #[test]
    fn await_agents_result_carries_failed_status() {
        let json = json!({
            "kind": "TydeAwaitAgents",
            "ready": [{ "agent_id": "agent-9", "status": "failed" }],
            "still_thinking": []
        });
        let decoded: ToolExecutionResult = serde_json::from_value(json).expect("deserialize");
        let ToolExecutionResult::TydeAwaitAgents {
            ready,
            still_thinking,
        } = decoded
        else {
            panic!("expected TydeAwaitAgents result");
        };
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].status, AgentControlStatus::Failed);
        assert!(still_thinking.is_empty());
    }
}

#[cfg(test)]
mod release_version_back_compat_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn welcome_payload_deserializes_without_release_version() {
        // Legacy hosts emit no `release_version`; it must default to None.
        let legacy = json!({
            "protocol_version": PROTOCOL_VERSION,
            "tyde_version": { "major": 0, "minor": 8, "patch": 14 },
        });
        let payload: WelcomePayload = serde_json::from_value(legacy).expect("deserialize legacy");
        assert_eq!(payload.release_version, None);
    }

    #[test]
    fn reject_payload_deserializes_without_release_version() {
        let legacy = json!({
            "code": "incompatible_protocol",
            "message": "nope",
            "server_protocol_version": PROTOCOL_VERSION,
            "server_tyde_version": { "major": 0, "minor": 8, "patch": 14 },
        });
        let payload: RejectPayload = serde_json::from_value(legacy).expect("deserialize legacy");
        assert_eq!(payload.release_version, None);
    }

    #[test]
    fn welcome_payload_round_trips_some_release_version_and_omits_none() {
        let version = TydeReleaseVersion::parse("0.8.19-beta.2").expect("valid version");
        let payload = WelcomePayload {
            protocol_version: PROTOCOL_VERSION,
            tyde_version: TYDE_VERSION,
            release_version: Some(version.clone()),
        };
        let encoded = serde_json::to_value(&payload).expect("serialize");
        assert_eq!(encoded["release_version"], json!("0.8.19-beta.2"));
        let decoded: WelcomePayload = serde_json::from_value(encoded).expect("round-trip");
        assert_eq!(decoded.release_version, Some(version));

        // `skip_serializing_if = "Option::is_none"` must omit the field entirely.
        let none = WelcomePayload {
            protocol_version: PROTOCOL_VERSION,
            tyde_version: TYDE_VERSION,
            release_version: None,
        };
        let encoded_none = serde_json::to_value(&none).expect("serialize none");
        assert!(encoded_none.get("release_version").is_none());
    }

    #[test]
    fn reject_payload_round_trips_some_release_version_and_omits_none() {
        let version = TydeReleaseVersion::parse("0.8.20-beta.1").expect("valid version");
        let payload = RejectPayload {
            code: RejectCode::IncompatibleProtocol,
            message: "drift".to_owned(),
            server_protocol_version: PROTOCOL_VERSION,
            server_tyde_version: TYDE_VERSION,
            release_version: Some(version.clone()),
        };
        let encoded = serde_json::to_value(&payload).expect("serialize");
        assert_eq!(encoded["release_version"], json!("0.8.20-beta.1"));
        let decoded: RejectPayload = serde_json::from_value(encoded).expect("round-trip");
        assert_eq!(decoded.release_version, Some(version));

        let none = RejectPayload {
            code: RejectCode::IncompatibleProtocol,
            message: "drift".to_owned(),
            server_protocol_version: PROTOCOL_VERSION,
            server_tyde_version: TYDE_VERSION,
            release_version: None,
        };
        let encoded_none = serde_json::to_value(&none).expect("serialize none");
        assert!(encoded_none.get("release_version").is_none());
    }
}

#[cfg(test)]
mod agent_control_output_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn latest_output_result_round_trips_typed_variants() {
        let cases = [
            AgentControlReadResult {
                agent_id: AgentId("agent-1".to_owned()),
                output: AgentControlOutput::Empty,
            },
            AgentControlReadResult {
                agent_id: AgentId("agent-1".to_owned()),
                output: AgentControlOutput::Message {
                    text: "visible answer".to_owned(),
                },
            },
            AgentControlReadResult {
                agent_id: AgentId("agent-1".to_owned()),
                output: AgentControlOutput::Error {
                    error: AgentErrorPayload {
                        agent_id: AgentId("agent-1".to_owned()),
                        code: AgentErrorCode::Internal,
                        message: "backend failed".to_owned(),
                        fatal: true,
                    },
                },
            },
        ];

        for expected in cases {
            let encoded = serde_json::to_value(&expected).expect("serialize read result");
            let decoded: AgentControlReadResult =
                serde_json::from_value(encoded).expect("deserialize read result");
            assert_eq!(decoded, expected);
        }
    }

    #[test]
    fn message_output_contains_only_visible_text() {
        let result = AgentControlReadResult {
            agent_id: AgentId("agent-1".to_owned()),
            output: AgentControlOutput::Message {
                text: "visible answer".to_owned(),
            },
        };

        assert_eq!(
            serde_json::to_value(result).expect("serialize read result"),
            json!({
                "agent_id": "agent-1",
                "output": {
                    "kind": "message",
                    "text": "visible answer"
                }
            })
        );
    }

    fn assistant_message(content: &str) -> ChatMessage {
        ChatMessage {
            message_id: None,
            timestamp: 1,
            sender: MessageSender::Assistant {
                agent: "worker".to_owned(),
            },
            content: content.to_owned(),
            reasoning: None,
            tool_calls: Vec::new(),
            model_info: None,
            token_usage: None,
            context_breakdown: None,
            images: None,
        }
    }

    #[test]
    fn latest_output_state_observes_records_in_source_order_without_lookback() {
        let stream = StreamPath("/agent/agent-1".to_owned());
        let message = Envelope::from_payload(
            stream.clone(),
            FrameKind::ChatEvent,
            1,
            &ChatEvent::MessageAdded(assistant_message("visible")),
        )
        .expect("message envelope");
        let empty = Envelope::from_payload(
            stream.clone(),
            FrameKind::ChatEvent,
            2,
            &ChatEvent::MessageAdded(assistant_message("")),
        )
        .expect("empty envelope");
        let unrelated = Envelope::from_payload(
            stream.clone(),
            FrameKind::ChatEvent,
            3,
            &ChatEvent::TypingStatusChanged(false),
        )
        .expect("typing envelope");
        let error = AgentErrorPayload {
            agent_id: AgentId("agent-1".to_owned()),
            code: AgentErrorCode::BackendFailed,
            message: "failed".to_owned(),
            fatal: true,
        };
        let error_envelope = Envelope::from_payload(stream, FrameKind::AgentError, 4, &error)
            .expect("error envelope");

        let mut state = AgentControlLatestOutput::default();
        state
            .observe_event_log(&[message, empty, unrelated, error_envelope])
            .expect("project source-ordered output");
        assert_eq!(state.output(), &AgentControlOutput::Error { error });
    }

    #[test]
    fn debug_result_and_byte_cap_share_one_serialized_shape() {
        let event = Envelope::from_payload(
            StreamPath("/agent/agent-1".to_owned()),
            FrameKind::ChatEvent,
            7,
            &ChatEvent::MessageAdded(assistant_message("visible")),
        )
        .expect("message envelope");
        let capped = cap_agent_control_events(vec![event], 1024 * 1024, Some(6))
            .expect("typed envelope sizing");
        let result = AgentControlReadDebugResult {
            agent_id: AgentId("agent-1".to_owned()),
            events: capped.events,
            next_after_seq: capped.next_after_seq,
            max_bytes: 1024 * 1024,
            omitted_events: capped.omitted_events,
            omitted_event_bytes: capped.omitted_event_bytes,
        };
        let decoded: AgentControlReadDebugResult =
            serde_json::from_value(serde_json::to_value(&result).expect("serialize debug result"))
                .expect("deserialize debug result");
        assert_eq!(decoded, result);
    }

    #[test]
    fn bootstrap_requires_explicit_latest_output() {
        let error = serde_json::from_value::<AgentBootstrapPayload>(json!({ "events": [] }))
            .expect_err("bootstrap without latest_output must be rejected");
        assert!(error.to_string().contains("latest_output"));
    }
}

#[cfg(test)]
mod stream_identity_tests {
    use super::*;

    #[test]
    fn stream_identity_violation_round_trips_as_a_value_free_tag() {
        let violation = StreamIdentityViolation::ForeignActiveMessageId;
        let encoded = serde_json::to_value(violation).expect("serialize stream identity violation");
        assert_eq!(encoded, serde_json::json!("foreign_active_message_id"));
        let decoded: StreamIdentityViolation =
            serde_json::from_value(encoded).expect("deserialize stream identity violation");
        assert_eq!(decoded, violation);
    }

    #[test]
    fn legacy_stream_wire_frame_decodes_but_cannot_enter_runtime_without_an_identity() {
        let start: StreamStartData = serde_json::from_value(serde_json::json!({
            "agent": "assistant",
            "model": null,
        }))
        .expect("legacy frame remains decodable");
        assert_eq!(
            start.required_message_id(),
            Err(StreamIdentityViolation::MissingMessageId)
        );
    }

    #[test]
    fn server_generated_identity_is_deterministic_and_origin_tagged() {
        let origin =
            serde_json::to_value(ServerGeneratedChatMessageIdOrigin::IdlessProviderResponseItem)
                .expect("serialize origin");
        assert_eq!(origin, serde_json::json!("idless_provider_response_item"));
        let identity = ServerGeneratedChatMessageIdentity {
            origin: ServerGeneratedChatMessageIdOrigin::LegacyReplay,
            stream_epoch: 7,
            item_ordinal: 3,
        };
        assert_eq!(
            identity.message_id(),
            ChatMessageId("server-generated:legacy_replay:7:3".to_owned())
        );
        let round_trip: ServerGeneratedChatMessageIdentity = serde_json::from_value(
            serde_json::to_value(&identity).expect("serialize identity contract"),
        )
        .expect("deserialize identity contract");
        assert_eq!(round_trip, identity);
    }
}
