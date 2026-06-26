use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;

use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Prerelease-capable, traversal-safe release identifier used as the versioned
/// bundle key for the web/PWA client. Single source of truth lives in
/// `host-config`; re-exported here so wire payloads and downstream crates use
/// `protocol::TydeReleaseVersion`.
pub use host_config::{LOCAL_HOST_ID, TydeReleaseVersion};

pub const PROTOCOL_VERSION: u32 = 20;
pub const TYDE_VERSION: Version = Version {
    major: 0,
    minor: 8,
    patch: 14,
};
/// Shared MQTT-over-WebSocket-Secure endpoint reachable from both the native
/// host and the browser/PWA client (no mixed content; broker terminates TLS).
pub const DEFAULT_MOBILE_MQTT_BROKER_URL: &str = "wss://broker.emqx.io:8084/mqtt";

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

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
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
}

impl BackendKind {
    pub const fn supports_image_input(self) -> bool {
        match self {
            Self::Kiro | Self::Claude | Self::Codex => true,
            Self::Tycode | Self::Antigravity => false,
        }
    }
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
    MobileAccessState,
    MobilePairingOffer,
    ReviewCreate,
    ReviewAction,
    ReviewEvent,
    ReviewSubscribe,
    ProjectEvent,
    WorkflowNotify,
    WorkflowRunNotify,
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
            Self::MobileAccessState => f.write_str("mobile_access_state"),
            Self::MobilePairingOffer => f.write_str("mobile_pairing_offer"),
            Self::ReviewCreate => f.write_str("review_create"),
            Self::ReviewAction => f.write_str("review_action"),
            Self::ReviewEvent => f.write_str("review_event"),
            Self::ReviewSubscribe => f.write_str("review_subscribe"),
            Self::ProjectEvent => f.write_str("project_event"),
            Self::WorkflowNotify => f.write_str("workflow_notify"),
            Self::WorkflowRunNotify => f.write_str("workflow_run_notify"),
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
    pub sessions: Vec<SessionSummary>,
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
pub enum AgentsViewPreferencesUpdate {
    SetFilters { filters: AgentsViewFilters },
    SetSortMode { sort_mode: AgentSortMode },
    SetGroupMode { group_mode: AgentGroupMode },
    SetDensity { density: AgentListDensity },
    SetHideFinished { hide_finished: bool },
    SetManualOrder { manual_order: Vec<AgentOrderKey> },
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load_error: Option<AgentsViewPreferencesStoreError>,
    #[serde(default)]
    pub smart_views: AgentsSmartViewsSnapshot,
    #[serde(default)]
    pub tags: AgentTagsSnapshot,
    #[serde(default)]
    pub pins: AgentPinsSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentsViewPreferencesNotifyPayload {
    pub snapshot: AgentsViewPreferencesSnapshot,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentBootstrapPayload {
    pub events: Vec<AgentBootstrapEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "payload", rename_all = "snake_case")]
pub enum AgentBootstrapEvent {
    AgentStart(AgentStartPayload),
    AgentError(AgentErrorPayload),
    SessionSettings(SessionSettingsPayload),
    QueuedMessages(QueuedMessagesPayload),
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
    pub code_intel: CodeIntelSettings,
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
    CodeIntelLanguageServerPath {
        provider: CodeIntelProviderId,
        path: Option<HostExecutablePath>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostSettingsPayload {
    pub settings: HostSettings,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientErrorPayload {
    pub code: ClientErrorCode,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_context: Option<String>,
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MobileDeviceState {
    Paired,
    Connected,
    Revoked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MobileAccessErrorCode {
    InvalidConfig,
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
    Unsupported,
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
    Review { review_id: ReviewId },
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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ListSessionsPayload {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeleteSessionPayload {
    pub session_id: SessionId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: SessionId,
    pub backend_kind: BackendKind,
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionSettingValue {
    String(String),
    Bool(bool),
    Integer(i64),
    Null,
}

/// Current session settings values for an agent.
/// Keys match `SessionSettingField.key` from the schema.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewAgentPayload {
    pub agent_id: AgentId,
    pub name: String,
    pub origin: AgentOrigin,
    pub backend_kind: BackendKind,
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
    ReviewListChanged { reviews: Vec<ReviewSummary> },
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
    pub operation: String,
    pub code: CommandErrorCode,
    pub message: String,
    pub fatal: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentErrorPayload {
    pub agent_id: AgentId,
    pub code: AgentErrorCode,
    pub message: String,
    pub fatal: bool,
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
/// `StreamEnd` (possibly with a placeholder empty message) before the
/// next `StreamStart` on the same stream. `StreamDelta` /
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
///   1. If a stream is currently open, emit `StreamEnd` (with an empty
///      or error placeholder message) to close it.
///   2. Emit `ToolExecutionCompleted` for any outstanding
///      `ToolRequest`s the backend originated in this turn (mark them
///      unsuccessful / cancelled).
///   3. Emit exactly one `OperationCancelled`.
///   4. Emit `TypingStatusChanged(false)`.
///
/// This matches `tycode-core::chat::protocol::TurnProtocol::abort`.
/// Without step 1, the next turn's `StreamStart` violates the stream
/// pairing invariant above.
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
    pub token_usage: Option<TokenUsage>,
    pub context_breakdown: Option<ContextBreakdown>,
    pub images: Option<Vec<ImageData>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageMetadataUpdateData {
    pub message_id: ChatMessageId,
    pub model_info: Option<ModelInfo>,
    pub token_usage: Option<TokenUsage>,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub cached_prompt_tokens: Option<u64>,
    pub cache_creation_input_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
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
    pub message_id: Option<String>,
    pub agent: String,
    pub model: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamTextDeltaData {
    pub message_id: Option<String>,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamEndData {
    pub message: ChatMessage,
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
    Other {
        args: Value,
    },
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolExecutionCompletedData {
    pub tool_call_id: String,
    pub tool_name: String,
    pub tool_result: ToolExecutionResult,
    pub success: bool,
    pub error: Option<String>,
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
    fn protocol_version_is_twenty() {
        assert_eq!(PROTOCOL_VERSION, 20);
    }

    #[test]
    fn background_agent_settings_defaults_are_safe() {
        let settings: HostSettings = serde_json::from_str("{}").expect("deserialize settings");
        assert!(settings.background_agent_features.auto_generate_agent_names);
        assert!(!settings.background_agent_features.agent_activity_summaries);
        assert!(settings.code_intel.language_server_paths.is_empty());
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
    fn search_frame_kinds_display_snake_case() {
        assert_eq!(FrameKind::ProjectSearch.to_string(), "project_search");
        assert_eq!(
            FrameKind::ProjectSearchCancel.to_string(),
            "project_search_cancel"
        );
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
    fn project_file_contents_carries_version() {
        let payload = ProjectFileContentsPayload {
            path: ProjectPath {
                root: ProjectRootPath("/repo".to_owned()),
                relative_path: "src/main.rs".to_owned(),
            },
            version: ProjectFileVersion(7),
            contents: Some("fn main() {}".to_owned()),
            is_binary: false,
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
