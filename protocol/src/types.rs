use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;

use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PROTOCOL_VERSION: u32 = 2;
pub const TYDE_VERSION: Version = Version {
    major: 0,
    minor: 8,
    patch: 11,
};

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

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendKind {
    Tycode,
    Kiro,
    Claude,
    Codex,
    Gemini,
}

impl BackendKind {
    pub const fn supports_image_input(self) -> bool {
        match self {
            Self::Kiro | Self::Claude | Self::Codex => true,
            Self::Tycode | Self::Gemini => false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
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
    /// Spawned by the backend's own native sub-agent mechanism (e.g. Claude subagents).
    BackendNative,
    /// Spawned as a persistent member of a server-owned agent team.
    TeamMember,
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
    SpawnAgent,
    ListSessions,
    DeleteSession,
    SendMessage,
    EditQueuedMessage,
    CancelQueuedMessage,
    SendQueuedMessageNow,
    SetAgentName,
    Interrupt,
    CloseAgent,
    RunBackendSetup,
    ProjectCreate,
    ProjectRename,
    ProjectReorder,
    ProjectAddRoot,
    ProjectDeleteRoot,
    ProjectDelete,
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
    TeamMemberShuffle,
    TeamDraftCreate,
    TeamDraftUpdate,
    TeamDraftShuffle,
    TeamDraftApplyTemplate,
    TeamDraftCommit,
    TeamDraftDiscard,
    ProjectReadDiff,
    ProjectReadFile,
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

    SetSessionSettings,

    // Output events (server -> client)
    HostSettings,
    BackendSetup,
    NewAgent,
    AgentStart,
    AgentRenamed,
    AgentClosed,
    ChatEvent,
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
    TeamPresetCatalogNotify,
    TeamDraftNotify,
    TeamMemberShuffleSuggestionNotify,
    ProjectFileList,
    ProjectGitStatus,
    ProjectFileContents,
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
    ReviewCreate,
    ReviewAction,
    ReviewEvent,
    ReviewSubscribe,
    ProjectEvent,
}

impl fmt::Display for FrameKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Hello => f.write_str("hello"),
            Self::Welcome => f.write_str("welcome"),
            Self::Reject => f.write_str("reject"),
            Self::SetSetting => f.write_str("set_setting"),
            Self::SpawnAgent => f.write_str("spawn_agent"),
            Self::ListSessions => f.write_str("list_sessions"),
            Self::DeleteSession => f.write_str("delete_session"),
            Self::SendMessage => f.write_str("send_message"),
            Self::EditQueuedMessage => f.write_str("edit_queued_message"),
            Self::CancelQueuedMessage => f.write_str("cancel_queued_message"),
            Self::SendQueuedMessageNow => f.write_str("send_queued_message_now"),
            Self::SetAgentName => f.write_str("set_agent_name"),
            Self::Interrupt => f.write_str("interrupt"),
            Self::CloseAgent => f.write_str("close_agent"),
            Self::RunBackendSetup => f.write_str("run_backend_setup"),
            Self::ProjectCreate => f.write_str("project_create"),
            Self::ProjectRename => f.write_str("project_rename"),
            Self::ProjectReorder => f.write_str("project_reorder"),
            Self::ProjectAddRoot => f.write_str("project_add_root"),
            Self::ProjectDeleteRoot => f.write_str("project_delete_root"),
            Self::ProjectDelete => f.write_str("project_delete"),
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
            Self::TeamMemberShuffle => f.write_str("team_member_shuffle"),
            Self::TeamDraftCreate => f.write_str("team_draft_create"),
            Self::TeamDraftUpdate => f.write_str("team_draft_update"),
            Self::TeamDraftShuffle => f.write_str("team_draft_shuffle"),
            Self::TeamDraftApplyTemplate => f.write_str("team_draft_apply_template"),
            Self::TeamDraftCommit => f.write_str("team_draft_commit"),
            Self::TeamDraftDiscard => f.write_str("team_draft_discard"),
            Self::ProjectReadDiff => f.write_str("project_read_diff"),
            Self::ProjectReadFile => f.write_str("project_read_file"),
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
            Self::HostSettings => f.write_str("host_settings"),
            Self::BackendSetup => f.write_str("backend_setup"),
            Self::NewAgent => f.write_str("new_agent"),
            Self::AgentStart => f.write_str("agent_start"),
            Self::AgentRenamed => f.write_str("agent_renamed"),
            Self::AgentClosed => f.write_str("agent_closed"),
            Self::ChatEvent => f.write_str("chat_event"),
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
            Self::TeamPresetCatalogNotify => f.write_str("team_preset_catalog_notify"),
            Self::TeamDraftNotify => f.write_str("team_draft_notify"),
            Self::TeamMemberShuffleSuggestionNotify => {
                f.write_str("team_member_shuffle_suggestion_notify")
            }
            Self::ProjectFileList => f.write_str("project_file_list"),
            Self::ProjectGitStatus => f.write_str("project_git_status"),
            Self::ProjectFileContents => f.write_str("project_file_contents"),
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
            Self::ReviewCreate => f.write_str("review_create"),
            Self::ReviewAction => f.write_str("review_action"),
            Self::ReviewEvent => f.write_str("review_event"),
            Self::ReviewSubscribe => f.write_str("review_subscribe"),
            Self::ProjectEvent => f.write_str("project_event"),
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
    pub bootstrap: BootstrapData,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BootstrapData {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostSettings {
    #[serde(default)]
    pub enabled_backends: Vec<BackendKind>,
    #[serde(default)]
    pub default_backend: Option<BackendKind>,
    #[serde(default)]
    pub tyde_debug_mcp_enabled: bool,
    #[serde(default = "default_agent_control_mcp_enabled")]
    pub tyde_agent_control_mcp_enabled: bool,
}

fn default_agent_control_mcp_enabled() -> bool {
    true
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
    TydeDebugMcpEnabled {
        enabled: bool,
    },
    TydeAgentControlMcpEnabled {
        enabled: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostSettingsPayload {
    pub settings: HostSettings,
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendMessagePayload {
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub images: Option<Vec<ImageData>>,
    #[serde(default)]
    pub origin: Option<MessageOrigin>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MessageOrigin {
    User,
    Review { review_id: ReviewId },
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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InterruptPayload {}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CloseAgentPayload {}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ListSessionsPayload {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeleteSessionPayload {
    pub session_id: SessionId,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    pub created_at_ms: u64,
    pub instance_stream: StreamPath,
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
    pub roots: Vec<String>,
    #[serde(default)]
    pub sort_order: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectCreatePayload {
    pub name: String,
    pub roots: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectRenamePayload {
    pub id: ProjectId,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectReorderPayload {
    pub project_ids: Vec<ProjectId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectAddRootPayload {
    pub id: ProjectId,
    pub root: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectDeleteRootPayload {
    pub id: ProjectId,
    pub root: String,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProjectEventPayload {
    ReviewListChanged { reviews: Vec<ReviewSummary> },
}

#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(transparent)]
pub struct ProjectRootPath(pub String);

impl fmt::Display for ProjectRootPath {
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
    /// `git diff HEAD` — staged + unstaged combined. Used by Review.
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectFileListPayload {
    #[serde(default)]
    pub incremental: bool,
    pub roots: Vec<ProjectRootListing>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectRootListing {
    pub root: ProjectRootPath,
    pub entries: Vec<ProjectFileEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectGitStatusPayload {
    pub roots: Vec<ProjectRootGitStatus>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectRootGitStatus {
    pub root: ProjectRootPath,
    pub branch: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub clean: bool,
    pub files: Vec<ProjectGitFileStatus>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectFileContentsPayload {
    pub path: ProjectPath,
    pub contents: Option<String>,
    pub is_binary: bool,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReviewDiffSelection {
    /// v1 default. All uncommitted changes across all roots in the project.
    AllUncommitted,
    /// v2. One root, optionally narrowed to a path.
    Root {
        root: ProjectRootPath,
        scope: ProjectDiffScope,
        path: Option<String>,
    },
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
    pub body: String,
    pub source: ReviewCommentSource,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ReviewSuggestionState {
    Pending,
    Accepted { comment_id: ReviewCommentId },
    Rejected,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewCreatePayload {
    pub origin_agent_id: AgentId,
    pub selection: ReviewDiffSelection,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewSubscribePayload {}

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
        backend_kind: BackendKind,
        cost_hint: Option<SpawnCostHint>,
        instructions: Option<String>,
    },
    Submit,
    Cancel,
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
    Error { error: ReviewErrorPayload },
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
    OriginAgentNotRunning,
    AmbiguousOriginSession,
    ReviewerAlreadyRunning,
    ReviewerBackendUnsupported,
    GitFailed,
    IoFailed,
    Internal,
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
    Cancel,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewSummary {
    pub id: ReviewId,
    pub status: ReviewStatus,
    pub origin_session_id: SessionId,
    pub origin_agent_id: AgentId,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub user_comment_count: u32,
    pub pending_suggestion_count: u32,
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
    /// Initial directory to list. None = server chooses (its home directory).
    pub initial: Option<HostAbsPath>,
    pub include_hidden: bool,
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostBrowseOpenedPayload {
    pub home: HostAbsPath,
    pub root: HostAbsPath,
    pub separator: char,
    pub platform: HostPlatform,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostBrowseEntriesPayload {
    pub path: HostAbsPath,
    pub parent: Option<HostAbsPath>,
    pub entries: Vec<HostBrowseEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
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
/// Gemini, Kiro, Tycode) shares one contract.
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
    TypingStatusChanged(bool),
    StreamStart(StreamStartData),
    StreamDelta(StreamTextDeltaData),
    StreamReasoningDelta(StreamTextDeltaData),
    StreamEnd(StreamEndData),
    ToolRequest(ToolRequest),
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
    Other {
        args: Value,
    },
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
