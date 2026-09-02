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

pub const PROTOCOL_VERSION: u32 = 55;
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

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
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
    /// Legacy serialized value retained so stores written by older releases
    /// remain readable. Tycode is no longer offered or runnable as a backend.
    Tycode,
    /// Kiro. It reaches the agent over the Agent Client Protocol, and the
    /// binary, args and quirks adapter come from the session's launch profile
    /// via [`AcpAgentSpec`] — so any other ACP-speaking agent can be pointed at
    /// this backend — but the backend is named for the agent it is actually
    /// run with, and for the Kiro-specific behaviour worth reading out of it.
    ///
    /// Serialized as `"kiro"`. `"acp"` — the spelling this kind carried while
    /// it was named for the protocol — is still accepted on read, and the store
    /// migrations rewrite it, so a store written by any released build loads.
    /// Emitting the new spelling is a wire break for an un-updated client,
    /// which is what the `PROTOCOL_VERSION` bump to 50 exists to catch.
    #[serde(alias = "acp")]
    Kiro,
    Claude,
    Codex,
    Antigravity,
    Hermes,
}

impl BackendKind {
    /// Coarse composer affordance: may this backend ever accept image input?
    ///
    /// For [`Self::Kiro`] the authoritative answer is per-session — it comes
    /// from `promptCapabilities.image` in the agent's `initialize` response,
    /// which isn't known until the session is live. This returns `true` so the
    /// composer offers attachment, and the ACP backend rejects an image sent to
    /// an agent that declared no image support with an explicit error rather
    /// than silently dropping it.
    pub const fn supports_image_input(self) -> bool {
        match self {
            Self::Kiro | Self::Claude | Self::Codex | Self::Hermes => true,
            Self::Tycode | Self::Antigravity => false,
        }
    }
}

/// Serialized name the Kiro backend kind carried while it was named for the
/// protocol it speaks rather than the agent it runs. Only migrations should
/// reference this.
pub const LEGACY_ACP_BACKEND: &str = "acp";
/// Serialized name of the agent origin session forks carried before they
/// became ordinary top-level agents. Only migrations should reference this.
pub const LEGACY_SIDE_QUESTION_ORIGIN: &str = "side_question";
/// Serialized name of [`BackendKind::Kiro`].
pub const KIRO_BACKEND: &str = "kiro";
/// Launch profile id of the built-in Kiro agent. Reserved against
/// user-configured launch-profile ids.
///
/// Deliberately still spelled `acp:kiro` after the backend kind became `kiro`:
/// this is an opaque id persisted in every session record that ever used the
/// built-in profile, so changing it is its own migration with no user-visible
/// benefit. It is not derived from, and never compared against, the backend
/// kind's serialized name.
pub const KIRO_LAUNCH_PROFILE_ID: &str = "acp:kiro";

/// Which quirks implementation drives an ACP agent. Stock speaks the
/// specification only; other variants add agent-specific behavior that the
/// protocol does not cover (non-standard notification families, filesystem
/// session enumeration, stream sanitization).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum AcpAdapterId {
    /// Specification-only behavior. The correct choice for an unknown agent.
    #[default]
    Stock,
    Kiro,
}

/// How to launch one ACP agent. Carried by a launch profile whose
/// `backend_kind` is [`BackendKind::Kiro`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AcpAgentSpec {
    /// Executable to spawn. Resolved against the host PATH when not absolute.
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// Working directory override. Defaults to the session workspace root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Extra environment for the agent process. `BTreeMap` so persisted
    /// settings serialize in a stable order and diff cleanly.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub adapter: AcpAdapterId,
}

#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BackendAccessMode {
    #[default]
    Unrestricted,
    /// Adds guidance telling the agent not to mutate files or external state.
    /// This does not reduce backend permissions, remove tools, or reject MCP
    /// operations.
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
    SettingsWrite,
    BackendNativeSettingsWrite,
    InvokeSettingsAction,
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
    CancelBackgroundTask,
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
    BackendSettingsRefresh,
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
    MobilePushSubscribe,
    MobilePushUnsubscribe,
    ClientError,
    Heartbeat,
    VoiceStart,
    VoiceAudio,
    VoiceInputEnd,
    VoiceInterrupt,
    VoiceStop,

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
    SettingsWriteResult,
    AgentsViewPreferencesNotify,
    BackendSetup,
    NewAgent,
    AgentActivitySummary,
    AgentActivityStats,
    AgentTurnStateNotify,
    TaskTokenUsage,
    AgentStart,
    AgentRenamed,
    AgentCompactNotify,
    ContextCompactionNotify,
    ContextCompactionCapability,
    AgentClosed,
    ChatEvent,
    SessionHistory,
    AgentError,
    QueuedMessages,
    SessionList,
    SessionSummaryCountUpdated,
    ProjectNotify,
    CustomAgentNotify,
    SteeringNotify,
    SkillNotify,
    McpServerNotify,
    TeamNotify,
    TeamMemberNotify,
    TeamMemberBindingNotify,
    TeamCompactNotify,
    TeamContextCompactionNotify,
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
    VoiceCapabilities,
    VoiceAccepted,
    VoiceTranscript,
    VoiceState,
    VoiceOutput,
    VoiceError,
}

impl fmt::Display for FrameKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Hello => f.write_str("hello"),
            Self::Welcome => f.write_str("welcome"),
            Self::Reject => f.write_str("reject"),
            Self::SettingsWrite => f.write_str("settings_write"),
            Self::BackendNativeSettingsWrite => f.write_str("backend_native_settings_write"),
            Self::InvokeSettingsAction => f.write_str("invoke_settings_action"),
            Self::SettingsWriteResult => f.write_str("settings_write_result"),
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
            Self::CancelBackgroundTask => f.write_str("cancel_background_task"),
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
            Self::BackendSettingsRefresh => f.write_str("backend_settings_refresh"),
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
            Self::MobilePushSubscribe => f.write_str("mobile_push_subscribe"),
            Self::MobilePushUnsubscribe => f.write_str("mobile_push_unsubscribe"),
            Self::ClientError => f.write_str("client_error"),
            Self::Heartbeat => f.write_str("heartbeat"),
            Self::VoiceStart => f.write_str("voice_start"),
            Self::VoiceAudio => f.write_str("voice_audio"),
            Self::VoiceInputEnd => f.write_str("voice_input_end"),
            Self::VoiceInterrupt => f.write_str("voice_interrupt"),
            Self::VoiceStop => f.write_str("voice_stop"),
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
            Self::AgentTurnStateNotify => f.write_str("agent_turn_state_notify"),
            Self::TaskTokenUsage => f.write_str("task_token_usage"),
            Self::AgentStart => f.write_str("agent_start"),
            Self::AgentRenamed => f.write_str("agent_renamed"),
            Self::AgentCompactNotify => f.write_str("agent_compact_notify"),
            Self::ContextCompactionNotify => f.write_str("context_compaction_notify"),
            Self::ContextCompactionCapability => f.write_str("context_compaction_capability"),
            Self::AgentClosed => f.write_str("agent_closed"),
            Self::ChatEvent => f.write_str("chat_event"),
            Self::SessionHistory => f.write_str("session_history"),
            Self::AgentError => f.write_str("agent_error"),
            Self::QueuedMessages => f.write_str("queued_messages"),
            Self::SessionList => f.write_str("session_list"),
            Self::SessionSummaryCountUpdated => f.write_str("session_summary_count_updated"),
            Self::ProjectNotify => f.write_str("project_notify"),
            Self::CustomAgentNotify => f.write_str("custom_agent_notify"),
            Self::SteeringNotify => f.write_str("steering_notify"),
            Self::SkillNotify => f.write_str("skill_notify"),
            Self::McpServerNotify => f.write_str("mcp_server_notify"),
            Self::TeamNotify => f.write_str("team_notify"),
            Self::TeamMemberNotify => f.write_str("team_member_notify"),
            Self::TeamMemberBindingNotify => f.write_str("team_member_binding_notify"),
            Self::TeamCompactNotify => f.write_str("team_compact_notify"),
            Self::TeamContextCompactionNotify => f.write_str("team_context_compaction_notify"),
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
            Self::VoiceCapabilities => f.write_str("voice_capabilities"),
            Self::VoiceAccepted => f.write_str("voice_accepted"),
            Self::VoiceTranscript => f.write_str("voice_transcript"),
            Self::VoiceState => f.write_str("voice_state"),
            Self::VoiceOutput => f.write_str("voice_output"),
            Self::VoiceError => f.write_str("voice_error"),
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
pub struct HostBootstrapPayload<S = Value> {
    pub settings: S,
    /// Content-derived tag of the client-visible (secret-redacted) settings
    /// document in `settings`. Same derivation as
    /// [`HostSettingsPayload::etag`].
    #[serde(default)]
    pub settings_etag: String,
    /// Build-static JSON Schema (schemars, draft 2020-12) describing the
    /// host settings document. Delivered only here, never per-write.
    #[serde(default)]
    pub settings_schema: Value,
    /// Currently-configured write-only (secret) settings values with their
    /// server-issued revision tokens. Secret values themselves are omitted
    /// from every outbound settings document; this side channel is how
    /// clients render "configured" and mint `Version` expectations without
    /// ever seeing the value. Also republished on every `HostSettings`
    /// fanout.
    #[serde(default)]
    pub configured_secrets: Vec<ConfiguredSecret>,
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
    /// Authoritative liveness after replaying `events`.
    pub turn_active: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "payload", rename_all = "snake_case")]
pub enum AgentBootstrapEvent {
    AgentStart(AgentStartPayload),
    AgentError(AgentErrorPayload),
    SessionSettings(SessionSettingsPayload),
    QueuedMessages(QueuedMessagesPayload),
    AgentActivityStats(AgentActivityStatsPayload),
    ContextCompaction(ContextCompactionNotifyPayload),
    ContextCompactionCapability(ContextCompactionCapabilityPayload),
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
            Self::ContextCompaction(_) => FrameKind::ContextCompactionNotify,
            Self::ContextCompactionCapability(_) => FrameKind::ContextCompactionCapability,
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

/// Prefix every supervisor-authored kick message carries. It keeps supervisor
/// turns visibly labeled in the transcript and lets the server count
/// consecutive supervisor kicks straight from the event log, with no
/// per-agent bookkeeping that could survive or miss restarts.
pub const SUPERVISOR_MESSAGE_PREFIX: &str = "[Tyde Supervisor] ";

/// Prefix of the visible notice the agent actor records when the supervisor
/// interrupts a stalled turn. It keeps the interrupt attributable in the
/// transcript and is how the supervision context reader tells the supervisor's
/// own cancel apart from a user pressing stop — the log is the only state that
/// distinction may live in, so a scheduler restart cannot desync it.
pub const SUPERVISOR_STALL_INTERRUPT_NOTICE_PREFIX: &str =
    "[Tyde Supervisor] Interrupted this turn after no progress for";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostSettingsPayload<S = Value> {
    pub settings: S,
    /// Content-derived tag of the client-visible settings state: the
    /// secret-redacted document plus the configured-secret tokens, so a
    /// secret-only change still advances the etag. Stateless with respect to
    /// server memory (the secret-token key persists beside the store), so
    /// restarts mint no spurious mismatches. Clients clear pending drafts
    /// only when a broadcast snapshot's etag equals the `current_etag` of
    /// their own [`SettingsWriteResultPayload`].
    #[serde(default)]
    pub etag: String,
    /// Current configured write-only (secret) values and their revision
    /// tokens. Published on every settings snapshot (bootstrap and fanout)
    /// so clients can render configured-status and mint valid `Version`
    /// expectations without ever seeing a secret value.
    #[serde(default)]
    pub configured_secrets: Vec<ConfiguredSecret>,
}

/// Client-minted id correlating a [`FrameKind::SettingsWrite`] command with
/// its requester-scoped [`FrameKind::SettingsWriteResult`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SettingsWriteId(pub String);

impl fmt::Display for SettingsWriteId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Client -> Server: apply a batch of path operations to the host settings
/// document, all-or-nothing. Every operation carries a mandatory
/// compare-and-swap precondition ([`SettingExpectation`]), so a stale client
/// gets a visible `Conflict` instead of silently clobbering a concurrent
/// edit. The server answers with a requester-scoped
/// [`SettingsWriteResultPayload`]; an applied write additionally fans out the
/// new document to every subscriber as [`FrameKind::HostSettings`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettingsWritePayload {
    pub write_id: SettingsWriteId,
    pub ops: Vec<SettingOp>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendNativeSettingsWritePayload {
    pub write_id: SettingsWriteId,
    pub backend: BackendKind,
    pub settings: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InvokeSettingsActionPayload {
    pub write_id: SettingsWriteId,
    pub backend: BackendKind,
    pub resource: String,
    pub action: String,
    #[serde(default)]
    pub arguments: Value,
}

/// Maximum operations one [`SettingsWritePayload`] may carry.
pub const SETTINGS_WRITE_MAX_OPS: usize = 64;

/// One path operation inside a [`SettingsWritePayload`]. Paths are RFC 6901
/// JSON pointers into the host settings document. `Remove` is explicit and
/// distinct from `Replace` with `null`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum SettingOp {
    Replace {
        path: String,
        value: Value,
        expected: SettingExpectation,
    },
    Remove {
        path: String,
        expected: SettingExpectation,
    },
}

impl SettingOp {
    pub fn path(&self) -> &str {
        match self {
            Self::Replace { path, .. } | Self::Remove { path, .. } => path,
        }
    }

    pub fn expected(&self) -> &SettingExpectation {
        match self {
            Self::Replace { expected, .. } | Self::Remove { expected, .. } => expected,
        }
    }
}

/// Mandatory per-path compare-and-swap precondition on a [`SettingOp`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SettingExpectation {
    /// The full expected current value at the path. `null` matches both an
    /// explicit null and an absent member. Rejected for secret-bearing paths
    /// (their values are never on the wire); use `Version`/`Absent` there.
    Value { value: Value },
    /// Value-free version token. For ordinary paths this is the content hash
    /// of the client-visible subtree (self-computable via
    /// `settings_model::version_token`). For secret-bearing paths it is the
    /// SERVER-ISSUED per-path revision token published in
    /// [`ConfiguredSecret::token`] — keyed over the true value, so it
    /// changes whenever the secret changes and is never derivable from a
    /// guessed value. Accepted on any path.
    Version { token: String },
    /// "Nothing is configured here": the current value must be absent or
    /// null. The value-free way to guard a first write to a secret path
    /// (which never appears in `configured_secrets` while unset).
    Absent,
}

/// One currently-configured write-only (secret) settings value: its RFC 6901
/// pointer and the server-issued, value-free revision token clients echo as
/// a [`SettingExpectation::Version`] when replacing or removing it. Secrets
/// that are not configured are not listed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfiguredSecret {
    pub pointer: String,
    pub token: String,
}

/// Server -> Client, delivered only to the connection that issued the
/// [`FrameKind::SettingsWrite`]. `applied` reflects the host settings store:
/// `false` means nothing was applied anywhere; `true` means the store
/// committed and fanned out, though `field_errors` may still carry
/// post-commit backend-propagation diagnostics ([`SettingsErrorCode::BackendRejected`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettingsWriteResultPayload {
    pub write_id: SettingsWriteId,
    pub applied: bool,
    /// The etag of the current client-visible settings document after this
    /// result: the new document's etag when applied, the unchanged current
    /// document's etag when rejected. Equals the `etag` of the
    /// [`HostSettingsPayload`] fanout an applied write produces.
    pub current_etag: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub field_errors: Vec<SettingsFieldError>,
}

/// One pointer-addressed failure inside a [`SettingsWriteResultPayload`].
/// Value-free: pointers and messages name fields, bounds, and enum kinds,
/// never submitted values or secrets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettingsFieldError {
    /// RFC 6901 pointer into the settings document; `""` is document-wide.
    pub pointer: String,
    pub code: SettingsErrorCode,
    pub message: String,
}

/// Closed set of settings-write failure categories. Deliberately an enum,
/// not open strings: clients branch on these.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettingsErrorCode {
    /// A compare-and-swap precondition failed: the current value at the
    /// pointer is not what the client expected (concurrent edit).
    Conflict,
    /// The submitted value is malformed, out of range, or violates a
    /// cross-field invariant.
    Invalid,
    /// The pointer names no path the settings schema knows.
    UnknownPath,
    /// The operation would replace an ancestor of a secret-bearing value, or
    /// carried a value expectation for a secret-bearing path.
    SecretSubtree,
    /// An environment-dependent check could not be satisfied (e.g. a live
    /// backend schema needed for validation is unavailable).
    Unavailable,
    /// The store committed but propagating the change to a backend failed.
    BackendRejected,
    /// Two operations in the write target overlapping paths, or an operation
    /// targets a path another operation's write would implicitly normalize.
    OverlapRejected,
}

/// Parses an RFC 6901 JSON pointer into its unescaped reference tokens.
/// Returns `None` for syntactically invalid pointers (missing leading `/`,
/// dangling `~` escape). The empty pointer (`""`, the whole document) parses
/// to an empty token list.
pub fn parse_json_pointer(pointer: &str) -> Option<Vec<String>> {
    if pointer.is_empty() {
        return Some(Vec::new());
    }
    if !pointer.starts_with('/') {
        return None;
    }
    let mut tokens = Vec::new();
    for raw in pointer[1..].split('/') {
        let mut token = String::with_capacity(raw.len());
        let mut chars = raw.chars();
        while let Some(ch) = chars.next() {
            if ch == '~' {
                match chars.next() {
                    Some('0') => token.push('~'),
                    Some('1') => token.push('/'),
                    _ => return None,
                }
            } else {
                token.push(ch);
            }
        }
        tokens.push(token);
    }
    Some(tokens)
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
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
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
    /// `agy -p "/usage"`, which answers without starting a turn or spending
    /// quota.
    AntigravityUsageCommand,
    /// `kiro-cli-chat chat --agent-engine v1 --no-interactive /usage`, which
    /// reports subscription credits without starting a model turn.
    KiroUsageCommand,
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
    Codex {
        slot: CodexLimitSlot,
    },
    Claude {
        limit: ClaudeLimitType,
    },
    ClaudeModel {
        name: String,
    },
    /// Antigravity names its own buckets (`gemini-weekly`, `3p-5h`, …) and the
    /// set differs per account tier, so the vendor's id is carried verbatim
    /// rather than mapped onto a fixed enum that would silently drop a bucket
    /// the account actually has.
    Antigravity {
        bucket: String,
    },
    Kiro {
        bucket: String,
    },
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
    CreditUsage {
        used: String,
        limit: String,
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
            Self::CreditUsage { provenance, .. } if provenance.vendor_reported => {
                Some(PercentValueProvenance::VendorReported)
            }
            Self::UsedPercent { .. } | Self::CreditUsage { .. } => {
                Some(PercentValueProvenance::DerivedFromVendorTotals)
            }
            _ => None,
        }
    }

    pub fn remaining_percent_provenance(&self) -> Option<PercentValueProvenance> {
        matches!(self, Self::UsedPercent { .. } | Self::CreditUsage { .. })
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
    /// Non-fatal diagnostics from a ready backend-native settings operation.
    /// They remain typed so renderers never infer settings safety from text.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub advisories: Vec<BackendNativeSettingsAdvisory>,
}

/// Non-fatal, server-classified advisory associated with a native settings
/// snapshot. A `Ready` snapshot may carry one or more advisories.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BackendNativeSettingsAdvisory {
    NoProviderConfigured { message: String },
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

/// Web Push endpoint minted by the device's push service. Holding it together
/// with the matching VAPID private key is sufficient to deliver a notification
/// to that device, so it is redacted from `Debug` like any other capability.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PushEndpointUrl(pub String);

impl fmt::Debug for PushEndpointUrl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PushEndpointUrl(<redacted>)")
    }
}

/// Device's P-256 public key (`p256dh`), base64url, uncompressed SEC1 point.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PushPublicKey(pub String);

/// Device's 16-byte `auth` secret, base64url. Input to the RFC 8291 key
/// derivation, so it is a secret.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PushAuthSecret(pub String);

impl fmt::Debug for PushAuthSecret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PushAuthSecret(<redacted>)")
    }
}

/// VAPID application-server P-256 public key, base64url, uncompressed SEC1.
/// The device passes this to `pushManager.subscribe`, which binds the
/// subscription to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct VapidPublicKey(pub String);

/// VAPID P-256 private scalar, base64url. The device generates the pair and
/// shares the private half with every host it is paired to, so one push
/// subscription serves all of them; a browser allows only one subscription per
/// service worker registration, bound to a single application server key.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct VapidPrivateKey(pub String);

impl fmt::Debug for VapidPrivateKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("VapidPrivateKey(<redacted>)")
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MobilePushSubscription {
    pub endpoint: PushEndpointUrl,
    pub p256dh: PushPublicKey,
    pub auth: PushAuthSecret,
    pub vapid_public_key: VapidPublicKey,
    pub vapid_private_key: VapidPrivateKey,
}

impl fmt::Debug for MobilePushSubscription {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MobilePushSubscription")
            .field("endpoint", &self.endpoint)
            .field("p256dh", &self.p256dh)
            .field("auth", &self.auth)
            .field("vapid_public_key", &self.vapid_public_key)
            .field("vapid_private_key", &self.vapid_private_key)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MobilePushSubscribePayload {
    pub subscription: MobilePushSubscription,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MobilePushUnsubscribePayload {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MobilePushState {
    /// No subscription registered for this device.
    Disabled,
    /// A subscription is registered and has not been rejected.
    Enabled,
    /// The push service reported the subscription gone. The device re-subscribes
    /// on its next connect; until then it receives nothing.
    Expired,
}

/// Body of a push message, encrypted by the host under the subscription's own
/// keys and decrypted by the browser before the service worker sees it. The
/// push service relays ciphertext it cannot read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MobilePushNotification {
    pub agent_id: AgentId,
    pub agent_name: String,
    pub host_label: String,
    pub reason: MobilePushReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MobilePushReason {
    TurnComplete,
    QuestionPending,
    PlanApproval,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MobileAccessStatePayload {
    pub broker_status: MobileBrokerStatus,
    pub pairing: MobilePairingState,
    pub paired_devices: Vec<MobileDeviceSummary>,
    #[serde(default)]
    pub direct_hosting: MobileDirectHostingStatus,
}

/// State of the host's own mobile web server, which serves the loader shell and
/// app bundles over HTTP instead of tunnelling them through the managed
/// service. Reported so a bad bundle directory or an unavailable port is
/// visible in the Mobile settings tab rather than only in the host log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MobileDirectHostingStatus {
    #[default]
    Disabled,
    Online {
        bind_addr: String,
        asset_count: u32,
    },
    Error {
        message: String,
    },
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
    pub push: MobilePushState,
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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        access_mode: Option<BackendAccessMode>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
    AskUserQuestion {
        tool_call_id: String,
        answer: String,
    },
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

/// Stop a background command that is still running, identified by the card it
/// is running on. The card is the only handle the user has on it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CancelBackgroundTaskPayload {
    pub tool_call_id: String,
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

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CompactionOperationId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CompactionObservationId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HistoryPageRequestId(pub String);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionTrigger {
    UserRequested,
    UserTyped,
    TeamRequested,
    SupervisorRequested,
    BackendAutomatic,
    BackendObservedManual,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionMethod {
    NativeTextCommand,
    NativeRpc,
    InlineFallback,
    BackendAutomatic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionMutation {
    NotObserved,
    Completed,
    MayHaveMutated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionStage {
    WaitingForIdle,
    Dispatching,
    Compacting,
    Finalizing,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionMetrics {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_messages: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_messages: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub messages_summarized: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cumulative_dropped_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub precomputed: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ContextCompactionStatus {
    Deferred {
        stage: CompactionStage,
    },
    Started {
        stage: CompactionStage,
    },
    Progress {
        stage: CompactionStage,
    },
    Completed,
    Failed {
        accepted: bool,
        mutation: CompactionMutation,
    },
}

impl ContextCompactionStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Completed | Self::Failed { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextCompactionNotifyPayload {
    pub operation_id: CompactionOperationId,
    pub agent_id: AgentId,
    pub logical_session_id: SessionId,
    pub backend_kind: BackendKind,
    pub trigger: CompactionTrigger,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<CompactionMethod>,
    pub status: ContextCompactionStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_version: Option<String>,
    #[serde(default)]
    pub metrics: CompactionMetrics,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestedCompactionRoute {
    NativePreferred,
    InlineFallbackOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionAvailabilityReason {
    CapabilityStillDetermining,
    NativeTriggerUnavailable,
    BackendAutomaticOnly,
    InlineFallbackDisabled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RequestedCompactionAvailability {
    Available {
        route: RequestedCompactionRoute,
    },
    Determining {
        reason: CompactionAvailabilityReason,
    },
    AutomaticOnly {
        reason: CompactionAvailabilityReason,
    },
    Unavailable {
        reason: CompactionAvailabilityReason,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextCompactionCapabilityPayload {
    pub agent_id: AgentId,
    pub logical_session_id: SessionId,
    pub availability: RequestedCompactionAvailability,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextCompactionTimelineStatus {
    Completed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextCompactionTimelineEvent {
    pub marker_id: CompactionObservationId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<CompactionOperationId>,
    pub trigger: CompactionTrigger,
    pub method: CompactionMethod,
    pub backend_kind: BackendKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_session_id: Option<SessionId>,
    pub status: ContextCompactionTimelineStatus,
    pub mutation: CompactionMutation,
    #[serde(default)]
    pub metrics: CompactionMetrics,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    pub timestamp: u64,
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
    pub request_id: HistoryPageRequestId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_seq: Option<u64>,
    pub limit: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionHistoryPayload {
    pub agent_id: AgentId,
    pub request_id: HistoryPageRequestId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_before_seq: Option<u64>,
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
    /// The page size the server applied to **this response**. `None` means the
    /// server applied no bound and returned every session in scope.
    ///
    /// Distinct from [`ListSessionsPayload::limit`], where `None` means "no
    /// client-chosen limit — use this subscriber's replay-mode default", which
    /// resolves to unbounded only for a full-replay subscriber and to the
    /// mobile page size for a paged one.
    ///
    /// Clients echo this field verbatim when re-requesting the same view.
    /// Encoding "unbounded" as `total_count` made the server emit a value its
    /// own request validation rejects (`session list limit N exceeds maximum
    /// 128`), so a desktop host with more than
    /// [`MAX_SESSION_LIST_PAGE_LIMIT`] sessions failed every refresh.
    ///
    /// Invariant: [`SessionListPageStatus::More`] implies `Some(_)`. A page
    /// with no bound returned everything, so there is nothing left to continue
    /// with. Upheld by the host's own page construction; `ProtocolValidator`
    /// asserts it over observed frames in protocol and backend tests, so a
    /// regression fails a test rather than being rejected at runtime.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    pub total_count: u32,
    pub status: SessionListPageStatus,
}

impl Default for SessionListPageInfo {
    fn default() -> Self {
        Self {
            scope: SessionListScope::AllSessions,
            cursor: SessionListCursor::default(),
            limit: Some(DEFAULT_SESSION_LIST_PAGE_LIMIT),
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
    /// `None` means "no client-chosen limit — use this subscriber's
    /// replay-mode default". That is *not* the same as `None` on
    /// [`SessionListPageInfo::limit`], which describes a response that was
    /// actually unbounded. A full-replay subscriber resolves `None` to
    /// unbounded; a paged one resolves it to its own page size.
    ///
    /// Clients re-request a view by echoing the descriptor they were given, so
    /// the contract this field must uphold is that a subscriber accepts the
    /// limit it emitted — not that `None` means unbounded everywhere.
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
    /// Persisted assistant responses (one per `StreamEnd`, including partial
    /// responses followed by cancellation or failure); `message_count` is
    /// retained as the legacy wire name.
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummaryCountUpdatedPayload {
    pub session_id: SessionId,
    /// Persisted assistant responses for this session. Each `StreamEnd` counts,
    /// including a partial response followed by cancellation or failure.
    /// `assistant_turn_count` is retained as the existing wire field name.
    pub assistant_turn_count: u32,
    /// Authoritative last-activity timestamp persisted at the same response
    /// boundary.
    pub updated_at_ms: u64,
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

/// Host-stream liveness update for an agent whose instance stream the
/// subscriber has not attached. Once the stream is attached, `AgentBootstrap`
/// and the agent's own chat events are authoritative and this frame stops.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentTurnStateNotifyPayload {
    pub agent_id: AgentId,
    pub turn_active: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentActivityStats {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_output_line: Option<String>,
    #[serde(default)]
    pub tool_calls: u64,
    #[serde(default)]
    pub token_usage: TokenUsage,
    /// Authoritative aggregate when a backend reports only a total and no
    /// input/output split. Kept separate so zero-valued component fields are
    /// never mistaken for provider-reported zeros.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_usage_total_only: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_context_usage: Option<CurrentContextUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_context_breakdown: Option<ContextBreakdown>,
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
    /// Liveness when this descriptor was built, with the same meaning as
    /// `AgentBootstrapPayload::turn_active`. A client that has not attached
    /// the agent's instance stream has no other way to learn it.
    #[serde(default)]
    pub turn_active: bool,
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
    #[serde(default)]
    pub supports_parallel_tool_calls: bool,
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

/// Re-probe one backend's settings snapshot on demand. Backend settings are
/// otherwise only re-read after a save, so a change made outside Tyde (a
/// `hermes` CLI login, a hand-edited `config.yaml`) would sit stale until the
/// next save. Carries no values — the server republishes whatever it finds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendSettingsRefreshPayload {
    pub backend: BackendKind,
}

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
pub struct TeamMemberContextCompactionResult {
    pub agent_id: AgentId,
    pub logical_session_id: SessionId,
    pub operation_id: CompactionOperationId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<CompactionMethod>,
    pub status: ContextCompactionStatus,
    pub mutation: CompactionMutation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeamContextCompactionStatus {
    Started,
    Completed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamContextCompactionNotifyPayload {
    pub team_operation_id: CompactionOperationId,
    pub team_id: TeamId,
    pub status: TeamContextCompactionStatus,
    #[serde(default)]
    pub members: Vec<TeamMemberContextCompactionResult>,
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
    #[serde(default)]
    pub force: bool,
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

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProjectDiffRevision {
    #[default]
    WorkingTree,
    CommittedRange {
        base_oid: String,
        tip_oid: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffContextMode {
    Hunks,
    FullFile,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectReadDiffPayload {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    pub root: ProjectRootPath,
    pub scope: ProjectDiffScope,
    #[serde(default)]
    pub revision: ProjectDiffRevision,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_oid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub empty_tree_oid: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub clean: bool,
    pub files: Vec<ProjectGitFileStatus>,
    #[serde(default)]
    pub recent_commits: Vec<ProjectGitCommitSummary>,
    #[serde(default)]
    pub history_has_more: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectGitCommitSummary {
    pub oid: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_parent_oid: Option<String>,
    pub subject: String,
    pub author: String,
    pub authored_at_seconds: i64,
    pub is_merge: bool,
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
    Unmerged,
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
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    pub root: ProjectRootPath,
    pub scope: ProjectDiffScope,
    #[serde(default)]
    pub revision: ProjectDiffRevision,
    pub path: Option<String>,
    pub context_mode: DiffContextMode,
    pub files: Vec<ProjectGitDiffFile>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectGitDiffFile {
    pub relative_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change_kind: Option<ProjectGitChangeKind>,
    #[serde(default)]
    pub is_binary: bool,
    #[serde(default)]
    pub unmerged: bool,
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
    /// Legacy: stored records only. Committed changes are reviewed inside
    /// the workspace draft through `ReviewTarget::CommittedDiff` locations;
    /// creating a review with this selection is rejected.
    CommittedRange {
        root: ProjectRootPath,
        base_oid: String,
        tip_oid: String,
        commit_count: u32,
    },
}

impl ReviewDiffSelection {
    pub const fn kind_name(&self) -> &'static str {
        match self {
            Self::AllUncommitted => "all_uncommitted",
            Self::Workspace { .. } => "workspace",
            Self::Root { .. } => "root",
            Self::CommittedRange { .. } => "committed_range",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ReviewLocation {
    pub root: ProjectRootPath,
    pub relative_path: String,
    /// The exact surface whose contents this anchor describes. Older stored
    /// locations predate this discriminator and therefore remain unstaged.
    #[serde(default)]
    pub target: ReviewTarget,
    pub anchor: ReviewAnchor,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReviewTarget {
    #[default]
    UnstagedDiff,
    StagedDiff,
    CommittedDiff {
        base_oid: String,
        tip_oid: String,
    },
    /// A project text file. The revision is filled by the server when the
    /// comment is accepted; clients never provide canonical file contents.
    RegularFile {
        revision: String,
    },
}

impl ReviewTarget {
    pub const fn label(&self) -> &'static str {
        match self {
            Self::UnstagedDiff => "unstaged",
            Self::StagedDiff => "staged",
            Self::CommittedDiff { .. } => "committed",
            Self::RegularFile { .. } => "file",
        }
    }

    /// Whether two targets render on the same live editor/diff surface.
    /// Regular-file revisions intentionally share that surface so stale
    /// threads remain visible; use `Eq` when immutable snapshot identity is
    /// required, such as aggregate review rows.
    pub fn same_surface(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::UnstagedDiff, Self::UnstagedDiff)
            | (Self::StagedDiff, Self::StagedDiff)
            | (Self::RegularFile { .. }, Self::RegularFile { .. }) => true,
            (
                Self::CommittedDiff {
                    base_oid: left_base,
                    tip_oid: left_tip,
                },
                Self::CommittedDiff {
                    base_oid: right_base,
                    tip_oid: right_tip,
                },
            ) => left_base == right_base && left_tip == right_tip,
            _ => false,
        }
    }
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
    /// Server-authored immutable text snapshots used to validate and render
    /// regular-file review anchors. Snapshots remain frozen so a later edit
    /// makes an anchor stale instead of moving it to different text.
    #[serde(default)]
    pub file_snapshots: Vec<ReviewFileSnapshot>,
    pub comments: Vec<ReviewComment>,
    pub suggestions: Vec<ReviewSuggestedComment>,
    pub ai_reviewer: ReviewAiReviewerState,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewFileSnapshot {
    pub root: ProjectRootPath,
    pub relative_path: String,
    pub revision: String,
    pub lines: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewAiReviewerState {
    pub status: ReviewAiReviewerStatus,
    pub agent_id: Option<AgentId>,
    pub error: Option<String>,
    /// What the most recent AI reviewer was asked to read.
    #[serde(default)]
    pub scope: ReviewAiScope,
}

/// Which diff an AI reviewer reads. The review itself spans every target;
/// the reviewer needs one frozen diff to work from.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReviewAiScope {
    #[default]
    WorkingTree,
    CommittedRange {
        root: ProjectRootPath,
        base_oid: String,
        tip_oid: String,
    },
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
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
        #[serde(default)]
        scope: ReviewAiScope,
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
    /// The surface the counted feedback anchors to, so a committed-range
    /// comment on a path never badges that path's working-tree row.
    #[serde(default)]
    pub target: ReviewTarget,
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

pub const VOICE_PROTOCOL_VERSION: u16 = 2;
pub const MAX_VOICE_PACKETS_PER_FRAME: usize = 3;
pub const MAX_VOICE_AUDIO_BYTES: usize = 8 * 1024;
pub const VOICE_SESSION_MAX_SECONDS: u64 = 450;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct VoiceSessionId(pub String);

impl fmt::Display for VoiceSessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VoiceTarget {
    pub agent_id: AgentId,
    pub instance_stream: StreamPath,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VoiceAudioCodec {
    Opus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VoiceAudioFormat {
    pub codec: VoiceAudioCodec,
    pub sample_rate_hz: u32,
    pub channels: u8,
    pub frame_duration_ms: u16,
    pub target_bitrate_bps: u32,
}

impl VoiceAudioFormat {
    pub fn opus(sample_rate_hz: u32) -> Self {
        Self {
            codec: VoiceAudioCodec::Opus,
            sample_rate_hz,
            channels: 1,
            frame_duration_ms: 20,
            target_bitrate_bps: 24_000,
        }
    }
    pub fn valid(&self) -> bool {
        matches!(
            self.sample_rate_hz,
            8_000 | 12_000 | 16_000 | 24_000 | 48_000
        ) && self.channels == 1
            && self.frame_duration_ms == 20
            && (6_000..=64_000).contains(&self.target_bitrate_bps)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserAecStatus {
    Requested,
    Effective,
    Unavailable,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VoiceFormatPair {
    pub uplink: VoiceAudioFormat,
    pub downlink: VoiceAudioFormat,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum VoiceRequest {
    Conversation {
        target: VoiceTarget,
        formats: Vec<VoiceFormatPair>,
    },
    Dictation {
        formats: Vec<VoiceAudioFormat>,
    },
}

impl VoiceRequest {
    pub fn mode(&self) -> VoiceMode {
        match self {
            Self::Conversation { .. } => VoiceMode::Conversation,
            Self::Dictation { .. } => VoiceMode::Dictation,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VoiceMode {
    Conversation,
    Dictation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum VoiceAcceptedRequest {
    Conversation {
        target: VoiceTarget,
        uplink: VoiceAudioFormat,
        downlink: VoiceAudioFormat,
    },
    Dictation {
        uplink: VoiceAudioFormat,
    },
}

impl VoiceAcceptedRequest {
    pub fn mode(&self) -> VoiceMode {
        match self {
            Self::Conversation { .. } => VoiceMode::Conversation,
            Self::Dictation { .. } => VoiceMode::Dictation,
        }
    }

    pub fn valid(&self) -> bool {
        match self {
            Self::Conversation {
                uplink, downlink, ..
            } => uplink.valid() && downlink.valid(),
            Self::Dictation { uplink } => uplink.valid(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VoiceCapabilitiesPayload {
    pub protocol: u16,
    pub conversation_formats: Vec<VoiceFormatPair>,
    pub dictation_formats: Vec<VoiceAudioFormat>,
    pub max_batch_packets: u8,
    pub max_sessions_per_connection: u8,
    pub nova_available: bool,
    pub dictation_available: bool,
    pub native_capture: bool,
    pub native_aec: bool,
    pub browser_capture: bool,
    pub browser_aec: BrowserAecStatus,
    pub foreground_only: bool,
}

impl VoiceCapabilitiesPayload {
    pub fn for_connection(nova_available: bool, dictation_available: bool, desktop: bool) -> Self {
        Self {
            protocol: VOICE_PROTOCOL_VERSION,
            conversation_formats: vec![VoiceFormatPair {
                uplink: VoiceAudioFormat::opus(48_000),
                downlink: VoiceAudioFormat::opus(24_000),
            }],
            dictation_formats: vec![VoiceAudioFormat::opus(48_000)],
            max_batch_packets: 1,
            max_sessions_per_connection: 1,
            nova_available,
            dictation_available,
            native_capture: desktop,
            native_aec: desktop,
            browser_capture: !desktop,
            browser_aec: if desktop {
                BrowserAecStatus::Unavailable
            } else {
                BrowserAecStatus::Requested
            },
            foreground_only: !desktop,
        }
    }

    pub fn valid(&self) -> bool {
        self.protocol == VOICE_PROTOCOL_VERSION
            && !self.conversation_formats.is_empty()
            && self
                .conversation_formats
                .iter()
                .all(|pair| pair.uplink.valid() && pair.downlink.valid())
            && !self.dictation_formats.is_empty()
            && self.dictation_formats.iter().all(VoiceAudioFormat::valid)
            && (1..=MAX_VOICE_PACKETS_PER_FRAME as u8).contains(&self.max_batch_packets)
            && self.max_sessions_per_connection == 1
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VoiceStartPayload {
    pub generation: u64,
    pub request: VoiceRequest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VoiceAcceptedPayload {
    pub session_id: VoiceSessionId,
    pub generation: u64,
    pub request: VoiceAcceptedRequest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VoiceDirection {
    Input,
    Output,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VoiceAudioPayload {
    pub session_id: VoiceSessionId,
    pub generation: u64,
    pub direction: VoiceDirection,
    pub first_media_seq: u64,
    pub timestamp_samples_48k: u64,
    pub packet_lengths: Vec<u16>,
}

impl VoiceAudioPayload {
    pub fn validate_body(&self, body_len: usize) -> Result<(), &'static str> {
        if self.packet_lengths.is_empty() || self.packet_lengths.len() > MAX_VOICE_PACKETS_PER_FRAME
        {
            return Err("voice packet count out of range");
        }
        if body_len > MAX_VOICE_AUDIO_BYTES
            || self
                .packet_lengths
                .iter()
                .map(|v| usize::from(*v))
                .sum::<usize>()
                != body_len
        {
            return Err("voice body length mismatch");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VoiceSessionPayload {
    pub session_id: VoiceSessionId,
    pub generation: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VoiceTranscriptSpeaker {
    User,
    Assistant,
    Progress,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VoiceTranscriptPayload {
    pub session_id: VoiceSessionId,
    pub generation: u64,
    pub speaker: VoiceTranscriptSpeaker,
    pub text: String,
    pub is_final: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<ChatMessageId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VoiceSessionState {
    Starting,
    Listening,
    AgentWorking,
    Speaking,
    Interrupting,
    Ending,
    Ended,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VoiceStatePayload {
    pub session_id: VoiceSessionId,
    pub generation: u64,
    pub state: VoiceSessionState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_turn_id: Option<ModelTurnId>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VoiceFlowStats {
    pub admitted_packets: u64,
    pub dropped_packets: u64,
    pub admitted_bytes: u64,
    pub dropped_bytes: u64,
    pub queue_high_water_packets: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VoiceStopReason {
    UserExited,
    TargetChanged,
    ClientBackgrounded,
    PermissionLost,
    MediaFailed,
    TransportLost,
    AgentClosed,
    Inactivity,
    ServerShutdown,
    ProviderCompleted,
    ProviderFailed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VoiceStopPayload {
    pub session_id: VoiceSessionId,
    pub generation: u64,
    pub reason: VoiceStopReason,
    #[serde(default)]
    pub stats: VoiceFlowStats,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum VoiceUnavailableReason {
    NotEnabled,
    MissingCredentials,
    CredentialsExpired,
    RegionNotConfigured,
    ModelUnsupported,
    ServerAdapterUnavailable,
    ClientMediaUnavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum VoiceAvailability {
    Available,
    Unavailable { reason: VoiceUnavailableReason },
}

impl Default for VoiceAvailability {
    fn default() -> Self {
        Self::Unavailable {
            reason: VoiceUnavailableReason::NotEnabled,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VoiceErrorCode {
    InvalidRequest,
    AlreadyActive,
    NotAvailable,
    StaleGeneration,
    WrongTarget,
    InvalidAudio,
    ProviderUnavailable,
    CredentialsExpired,
    MissingCredentials,
    PermissionDenied,
    QuotaExceeded,
    InvalidConfiguration,
    ToolBusy,
    ToolDeliveryFailed,
    Inactivity,
    Internal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VoiceErrorPayload {
    pub session_id: Option<VoiceSessionId>,
    pub generation: u64,
    pub code: VoiceErrorCode,
    pub retryable: bool,
    pub fatal: bool,
    /// Human-readable provider failure text (e.g. the Bedrock
    /// ValidationException message). Never populated for credential
    /// failures, which stay typed-only so provider text cannot leak
    /// account diagnostics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    pub stream: StreamPath,
    pub request_kind: FrameKind,
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

/// Events emitted on an agent's ordered chat stream.
///
/// A provider turn is not a message boundary. One provider turn may contain
/// many model requests. Every `StreamStart` through `StreamEnd` pair is one
/// model request/response and produces exactly one assistant `ChatMessage`.
/// `OperationCancelled` may instead abort the one open response without
/// fabricating a partial assistant message.
///
/// `StreamDelta` and `StreamReasoningDelta` belong to the one currently open
/// response, so they do not carry message ids. `StreamEnd.message` is the
/// authoritative response and owns its text, reasoning, images, token usage,
/// context, and tool declarations.
///
/// Every `ToolRequest` must refer to a `ToolUseData` declaration from an
/// assistant response. `tool_call_id` is the only tool execution identity.
/// The tool's name and provider arguments live on the declaration and are not
/// repeated on progress or completion events.
///
/// A tool may move from foreground to background without becoming a different
/// task or receiving another id. It remains open until exactly one
/// `ToolExecutionCompleted`. No `ToolProgress` is valid after completion.
///
/// ### Activity
/// `TypingStatusChanged` is the authoritative foreground-turn activity signal.
/// Background tools, subagents, and workflows remain visible through progress
/// events and must not keep the foreground turn active.
/// A backend emits `false` as soon as it can accept another user turn, even
/// while detached work continues. Provider-initiated continuation starts a new
/// foreground turn and therefore emits its own paired activity signals.
///
/// ### Cancellation ordering
/// When foreground work is cancelled the backend must, in this order:
///   1. Abort any open response. Its partial deltas do not become a message.
///   2. Emit a cancelled `ToolExecutionCompleted` for each open foreground
///      tool. Calls already moved to `Background` continue independently.
///   3. Emit exactly one `OperationCancelled`.
///   4. Emit `TypingStatusChanged(false)`.
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
    /// Zero or more full progress snapshots for an open tool call.
    ToolProgress(ToolProgressData),
    ToolExecutionCompleted(ToolExecutionCompletedData),
    TaskUpdate(TaskList),
    OperationCancelled(OperationCancelledData),
    RetryAttempt(RetryAttemptData),
    Orchestration(OrchestrationEvent),
    ContextCompaction(ContextCompactionTimelineEvent),
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
    /// Tyde-owned presentation identity. Provider response ids and tool-call
    /// ids are never reused as message ids. Consumers still render a message
    /// when this is absent; it only loses addressable late metadata.
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
    /// Presentation-only correlation. Consumers ignore an update whose message
    /// is no longer known rather than rejecting the chat stream.
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
    pub tool_call_id: String,
    pub name: String,
    pub arguments: Value,
    /// Unicode scalar-value offset into the owning message's `content` at
    /// which the tool call was observed. Rust can reproduce it with
    /// `content.chars()` and JavaScript with `Array.from(content)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_offset: Option<u32>,
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

impl TokenUsage {
    /// The whole prompt the model read for this request. Backends normalize
    /// `input_tokens` to the uncached slice only (Claude reports cache reads
    /// and writes as separate counters; Codex subtracts its cached count to
    /// match), so a warm-cache request reads as a few dozen tokens unless the
    /// cache counters are added back.
    pub fn prompt_tokens(&self) -> u64 {
        self.input_tokens
            .saturating_add(self.cached_prompt_tokens.unwrap_or(0))
            .saturating_add(self.cache_creation_input_tokens.unwrap_or(0))
    }
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_context_usage: Option<CurrentContextUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_context_breakdown: Option<ContextBreakdown>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CurrentContextUsage {
    Unknown,
    Known {
        input_tokens: u64,
        context_window: u64,
    },
}

impl CurrentContextUsage {
    pub fn known(&self) -> Option<(u64, u64)> {
        match self {
            Self::Unknown => None,
            Self::Known {
                input_tokens,
                context_window,
            } => Some((*input_tokens, *context_window)),
        }
    }
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    pub agent: String,
    pub model: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamTextDeltaData {
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamEndData {
    /// The complete provider response. It must never be a synthetic message
    /// created merely to give a tool call a parent.
    pub message: ChatMessage,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolRequest {
    pub tool_call_id: String,
    /// The provider's own name for the tool, as declared by the owning
    /// response's `ToolUseData`. Carried here so a consumer can identify the
    /// tool without waiting for that response: backends disagree on whether
    /// the request or its declaring `StreamEnd` arrives first, and the server
    /// projects Tyde's own MCP tools onto typed requests by this name.
    ///
    /// Empty only for requests replayed from a session log written before this
    /// field existed.
    #[serde(default)]
    pub tool_name: String,
    /// Tyde's normalized executable form. The arguments are read from the
    /// owning response's `ToolUseData`.
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
        /// Whether the provider keeps this child attached to the invoking turn
        /// or lets it outlive that foreground turn. Older persisted requests
        /// predate this field and deserialize as `Unknown`; current backend
        /// emitters must supply an authoritative mode.
        #[serde(default)]
        execution_mode: AgentExecutionMode,
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentExecutionMode {
    Foreground,
    Background,
    #[default]
    #[serde(other)]
    Unknown,
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
    /// The same tool call may move from foreground to background. That changes
    /// scheduling, not identity.
    #[serde(default)]
    pub execution_mode: ToolExecutionMode,
    /// This exact tool call can be stopped on its own, so the card may offer
    /// cancel. Per call rather than per backend: a backend that can cancel in
    /// general still loses the handle for work it only observes second-hand.
    #[serde(default)]
    pub cancellable: bool,
    pub update: ToolProgressUpdate,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolExecutionMode {
    #[default]
    Foreground,
    Background,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolProgressUpdate {
    SubAgent(SubAgentProgress),
    Workflow(WorkflowRunState),
    AgentControl(AgentControlProgress),
    Other { payload: Value },
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
    #[serde(default)]
    pub status: SubAgentProgressStatus,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubAgentProgressStatus {
    Running,
    Completed,
    Failed,
    Stopped,
    #[default]
    #[serde(other)]
    Unknown,
}

/// Live Tyde agent-control MCP progress for tool cards that spawn or wait on
/// first-class Tyde agents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentControlProgress {
    pub progress_kind: AgentControlProgressKind,
    pub agents: Vec<AgentControlAgentRef>,
    #[serde(default)]
    pub status: AgentControlProgressStatus,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentControlProgressStatus {
    Running,
    Completed,
    Failed,
    Stopped,
    #[default]
    #[serde(other)]
    Unknown,
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
    pub outcome: ToolExecutionOutcome,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ToolExecutionOutcome {
    Succeeded {
        result: ToolExecutionResult,
    },
    Failed {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        normalization_failure: Option<ToolExecutionNormalizationFailure>,
    },
    Cancelled {
        message: String,
    },
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
