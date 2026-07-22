use std::cell::Cell;
use std::collections::{HashMap, HashSet, VecDeque};

use crate::bridge::{ConfiguredHost, RemoteHostLifecycleStatus};
use leptos::prelude::*;
use protocol::FrameKind;
use protocol::{
    AgentActivityStats, AgentActivitySummaryState, AgentGroupMode, AgentId, AgentListDensity,
    AgentOrderKey, AgentOrigin, AgentSortMode, AgentWorkflowMetadata, AgentsSidebarPreferences,
    AgentsViewFilters, AgentsViewPreferences, AgentsViewPreferencesSnapshot, BackendKind,
    BackendSetupInfo, ByteRange, ChatMessage, ChatMessageId, CodeIntelDiagnostic,
    CodeIntelErrorPayload, CodeIntelFileModelPayload, CodeIntelLocation, CodeIntelOccurrence,
    CodeIntelOverviewPayload, CodeIntelReferencesFileResult, CodeIntelStatusPayload, CustomAgent,
    CustomAgentId, DiffContextMode, GitBranchName, HostAbsPath, HostBrowseEntry,
    HostBrowseErrorPayload, HostPlatform, HostSettings, LaunchProfileCatalog, LaunchProfileId,
    McpServerConfig, McpServerId, MessageMetadataUpdateData, MobileAccessStatePayload,
    MobilePairingOfferPayload, Project, ProjectDiffScope, ProjectFileVersion, ProjectGitDiffFile,
    ProjectGitDiffPayload, ProjectId, ProjectPath, ProjectRootGitStatus, ProjectRootListing,
    ProjectRootPath, ProjectSearchFileResult, QueuedMessageEntry, Review, ReviewCommentId,
    ReviewId, ReviewSuggestionId, ReviewSummary, SessionId, SessionSchemaEntry,
    SessionSettingsValues, SessionSummary, Skill, SkillId, SmartViewId, Steering, SteeringId,
    StreamPath, TaskList, TaskTokenUsagePayload, Team, TeamDraft, TeamDraftId, TeamId, TeamMember,
    TeamMemberBindingPayload, TeamMemberId, TeamMemberShuffleSuggestion,
    TeamMemberShuffleSuggestionNotifyPayload, TeamPresetCatalog, TerminalId,
    ToolExecutionCompletedData, ToolProgressData, ToolRequest, WorkflowCatalogLocation,
    WorkflowDiagnostic, WorkflowId, WorkflowInputSpec, WorkflowRunId, WorkflowRunSnapshot,
    WorkflowSummary,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffViewMode {
    Unified,
    SideBySide,
}

/// How verbose tool-call cards render in the chat.
///
/// `Summary` collapses the body to header-only; `Compact` shows previews with
/// per-tool caps and an expand toggle; `Full` shows everything inline.
/// Persisted to `localStorage` via `persist_tool_output_mode` —
/// pure presentation, never sent over the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolOutputMode {
    Summary,
    Compact,
    Full,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ConnectionStatus {
    Disconnected,
    Connecting,
    Connected,
    Error(String),
}

/// In-flight/failed state for a backend-native settings save. See
/// [`AppState::native_settings_save_state`].
#[derive(Clone, Debug, PartialEq)]
pub enum NativeSettingsSaveState {
    /// A save is in flight. `base` is the settings document the save was applied
    /// to; the save is considered landed once the server publishes a snapshot
    /// whose settings document differs from `base`.
    Pending { base: serde_json::Value },
    /// The last save failed to send; carries a user-facing reason.
    Failed { message: String },
}

#[derive(Clone, Debug, PartialEq)]
pub struct AgentInfo {
    pub host_id: String,
    pub agent_id: AgentId,
    pub name: String,
    pub origin: AgentOrigin,
    pub backend_kind: BackendKind,
    pub workspace_roots: Vec<String>,
    pub project_id: Option<ProjectId>,
    pub parent_agent_id: Option<AgentId>,
    pub session_id: Option<SessionId>,
    pub custom_agent_id: Option<CustomAgentId>,
    pub workflow: Option<AgentWorkflowMetadata>,
    pub created_at_ms: u64,
    pub instance_stream: StreamPath,
    pub started: bool,
    /// Set when a fatal `AgentError` arrives. The agent is terminated and no
    /// further events will arrive on its stream.
    pub fatal_error: Option<String>,
    /// Server-owned background activity summary state. Rendered (when enabled)
    /// in surfaces like the await-agents tool card. Defaults to `Disabled`;
    /// the frontend never infers this — it mirrors server-emitted state from
    /// `NewAgentPayload.activity_summary` and `AgentActivitySummary` frames.
    pub activity_summary: AgentActivitySummaryState,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct AgentMonitorKey {
    pub host_id: String,
    pub agent_id: AgentId,
}

impl AgentMonitorKey {
    pub fn new(host_id: impl Into<String>, agent_id: AgentId) -> Self {
        Self {
            host_id: host_id.into(),
            agent_id,
        }
    }

    pub fn from_agent(agent: &AgentInfo) -> Self {
        Self::new(agent.host_id.clone(), agent.agent_id.clone())
    }
}

// ── Tab system ──────────────────────────────────────────────────────────

/// Maximum number of tab content components mounted at once. The active tab
/// is always mounted; the rest of the slots hold the most-recently-active
/// tabs before it (display:none, but state preserved for instant switch
/// back). Tabs beyond this hot set are fully unmounted — switching back
/// remounts them from cached AppState (chat_rows, open_files, diff_contents)
/// so no data is lost, only ephemeral UI state like scroll position.
pub const TAB_LRU_CAPACITY: usize = 2;
pub const CENTER_SPLIT_RATIO_STORAGE_KEY: &str = "tyde-center-split-ratio";
#[cfg(target_arch = "wasm32")]
const ACTIVE_PROJECT_STORAGE_KEY: &str = "tyde-active-project";

/// Id of the builtin "Default" custom agent. It backs every spawn that picks
/// no explicit agent, so pickers that already offer a "Default agent" row
/// hide this record to avoid a duplicate entry.
pub const DEFAULT_CUSTOM_AGENT_ID: &str = "tyde-default";

/// Configured-connection id of the primary local host. It is the only host
/// that owns and emits Agents-view preferences (dev-docs/26 §12.1); a `Some`
/// snapshot from any other host is ignored so a stray remote payload cannot
/// hijack the client-global preference signal or its owner pointer.
pub const PRIMARY_LOCAL_HOST_ID: &str = "local";

/// Safety backstop: if an optimistic Agents-view overlay is not reconciled by
/// an authoritative server snapshot within this window (e.g. the
/// `SetAgentsViewPreferences` send was dropped and no notify ever arrives), it
/// is dropped so a failed mutation can never freeze the view.
#[cfg(target_arch = "wasm32")]
const OVERLAY_RECONCILE_TIMEOUT_MS: i32 = 4000;

thread_local! {
    static NEXT_TAB_ID: Cell<u64> = const { Cell::new(0) };
}

#[cfg(target_arch = "wasm32")]
fn load_active_project() -> Option<ActiveProjectRef> {
    let storage = web_sys::window()?.local_storage().ok().flatten()?;
    let encoded = storage
        .get_item(ACTIVE_PROJECT_STORAGE_KEY)
        .ok()
        .flatten()?;
    match serde_json::from_str(&encoded) {
        Ok(project) => Some(project),
        Err(error) => {
            log::warn!("invalid persisted active project: {error}");
            let _ = storage.remove_item(ACTIVE_PROJECT_STORAGE_KEY);
            None
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn load_active_project() -> Option<ActiveProjectRef> {
    None
}

#[cfg(target_arch = "wasm32")]
fn persist_active_project(project: Option<&ActiveProjectRef>) {
    let Some(storage) = web_sys::window().and_then(|window| window.local_storage().ok().flatten())
    else {
        return;
    };
    match project {
        Some(project) => match serde_json::to_string(project) {
            Ok(encoded) => {
                if let Err(error) = storage.set_item(ACTIVE_PROJECT_STORAGE_KEY, &encoded) {
                    log::warn!("failed to persist active project: {error:?}");
                }
            }
            Err(error) => log::warn!("failed to encode active project: {error}"),
        },
        None => {
            let _ = storage.remove_item(ACTIVE_PROJECT_STORAGE_KEY);
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn persist_active_project(_project: Option<&ActiveProjectRef>) {}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TabId(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TabScrollState {
    pub scroll_top: i32,
    pub scroll_height: i32,
    pub client_height: i32,
    pub user_scrolled_up: bool,
}

pub fn next_tab_id() -> TabId {
    NEXT_TAB_ID.with(|cell| {
        let id = cell.get();
        cell.set(id + 1);
        TabId(id)
    })
}

/// A chat tab whose `agent_ref` has not yet been resolved because the user
/// opened a team member whose live binding does not exist yet. The first user
/// message sent in this tab is routed through `TeamMemberActivate` instead of
/// `SpawnAgent`, and the resulting `NewAgent` echo upgrades the tab's
/// `agent_ref` in place.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingTeamMember {
    pub host_id: String,
    pub member_id: TeamMemberId,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FileResourceKey {
    pub host_id: String,
    pub project_id: ProjectId,
    pub path: ProjectPath,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TabContent {
    Home,
    AgentMonitor,
    Chat {
        agent_ref: Option<ActiveAgentRef>,
        /// `Some` only while the user has opened a team member whose live
        /// agent hasn't been spawned yet. `None` for ordinary draft and live
        /// chat tabs — the discriminator that tells `submit_chat_input` to
        /// send `TeamMemberActivate` instead of `SpawnAgent::New`.
        pending_team_member: Option<PendingTeamMember>,
    },
    File {
        key: FileResourceKey,
    },
    Diff {
        /// Explicit owning project identity. Carried so a review overlay
        /// binds to the exact (host, project) the tab was opened for —
        /// resolving the project from `root` alone is ambiguous when two
        /// hosts/projects share the same root path string.
        host_id: String,
        project_id: ProjectId,
        root: ProjectRootPath,
        scope: ProjectDiffScope,
        path: String,
    },
    /// Compact review-comments surface for the project's single workspace
    /// draft review: snippets around each human comment, accepted AI comment,
    /// and pending AI suggestion — not the full diff — grouped by root. Binds
    /// to the explicit `(host_id, project_id)`; there is one active workspace
    /// review per project spanning every root.
    Comments {
        host_id: String,
        project_id: ProjectId,
    },
    /// Detail view for a Claude Code workflow run, opened from its tool
    /// card. Binds to the owning agent's chat plus the Workflow tool
    /// call id; live state is read from `AppState::workflow_runs`.
    Workflow {
        agent_ref: ActiveAgentRef,
        tool_call_id: ToolCallId,
    },
}

impl TabContent {
    pub fn empty_chat() -> Self {
        Self::Chat {
            agent_ref: None,
            pending_team_member: None,
        }
    }

    pub fn chat_with_agent(agent_ref: ActiveAgentRef) -> Self {
        Self::Chat {
            agent_ref: Some(agent_ref),
            pending_team_member: None,
        }
    }

    pub fn team_member_draft(host_id: String, member_id: TeamMemberId) -> Self {
        Self::Chat {
            agent_ref: None,
            pending_team_member: Some(PendingTeamMember { host_id, member_id }),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Tab {
    pub id: TabId,
    pub content: TabContent,
    pub label: String,
    pub closeable: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum BackingResource {
    File(FileResourceKey),
    Diff(DiffKey),
}

impl Tab {
    pub fn backing_resource(&self) -> Option<BackingResource> {
        match &self.content {
            TabContent::File { key } => Some(BackingResource::File(key.clone())),
            TabContent::Diff {
                host_id,
                project_id,
                root,
                scope,
                path,
            } => Some(BackingResource::Diff(DiffKey::new(
                host_id.clone(),
                project_id.clone(),
                root.clone(),
                *scope,
                path.clone(),
            ))),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PaneId {
    Primary,
    Secondary,
}

impl PaneId {
    pub fn other(self) -> Self {
        match self {
            Self::Primary => Self::Secondary,
            Self::Secondary => Self::Primary,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SplitRatio(f64);

impl SplitRatio {
    pub const MIN: f64 = 0.2;
    pub const MAX: f64 = 0.8;
    pub const DEFAULT: f64 = 0.5;

    pub fn new(value: f64) -> Self {
        if value.is_finite() {
            Self(value.clamp(Self::MIN, Self::MAX))
        } else {
            Self(Self::DEFAULT)
        }
    }

    pub fn get(self) -> f64 {
        self.0
    }
}

impl Default for SplitRatio {
    fn default() -> Self {
        Self::new(Self::DEFAULT)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenTarget {
    Focused,
    // Only tests construct `Beside` since the open-to-side controls were
    // removed; `resolve` and the open-path entry points still honor it.
    #[allow(dead_code)]
    Beside,
}

pub const CENTER_TABS_DISABLED_REASON: &str = "Enable tabs to use split view.";
pub const TAB_SOURCE_MISSING_REASON: &str = "This tab is no longer open.";
#[cfg(test)]
pub const DUPLICATE_FILE_TABS_DISABLED_REASON: &str = CENTER_TABS_DISABLED_REASON;
#[cfg(test)]
pub const DUPLICATE_FILE_SOURCE_MISSING_REASON: &str = TAB_SOURCE_MISSING_REASON;
#[cfg(test)]
pub const DUPLICATE_FILE_NOT_A_FILE_REASON: &str = "Only files can be split.";
#[cfg(test)]
pub const DUPLICATE_FILE_NOT_LOADED_REASON: &str = "Wait for the file to finish loading.";
#[cfg(test)]
pub const OPEN_TO_SIDE_CROSS_PROJECT_REASON: &str =
    "This resource is in another project — open that project first.";
#[cfg(test)]
pub const OPEN_TO_SIDE_NOTHING_WOULD_REMAIN_REASON: &str = "Nothing would be left in this pane.";
#[cfg(test)]
pub const AGENT_OPEN_TO_SIDE_CROSS_PROJECT_REASON: &str =
    "This agent is in another project — open that project first.";
pub const MOVE_ALREADY_IN_TARGET_PANE_REASON: &str = "This tab is already in that pane.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DuplicateFileEligibility {
    Enabled,
    TabsDisabled,
    SourceTabMissing,
    NotAFile,
    NotLoaded,
    TargetAlreadyContainsResource { existing: TabId },
}

impl DuplicateFileEligibility {
    #[cfg(test)]
    pub fn is_enabled(self) -> bool {
        matches!(
            self,
            Self::Enabled | Self::TargetAlreadyContainsResource { .. }
        )
    }

    #[cfg(test)]
    pub fn disabled_reason(self) -> Option<&'static str> {
        match self {
            Self::Enabled | Self::TargetAlreadyContainsResource { .. } => None,
            Self::TabsDisabled => Some(DUPLICATE_FILE_TABS_DISABLED_REASON),
            Self::SourceTabMissing => Some(DUPLICATE_FILE_SOURCE_MISSING_REASON),
            Self::NotAFile => Some(DUPLICATE_FILE_NOT_A_FILE_REASON),
            Self::NotLoaded => Some(DUPLICATE_FILE_NOT_LOADED_REASON),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DuplicateFileResult {
    Duplicated {
        source: TabId,
        tab: TabId,
        target: PaneId,
    },
    ActivatedExisting {
        source: TabId,
        existing: TabId,
        target: PaneId,
    },
    TabsDisabled,
    SourceTabMissing,
    NotAFile,
    NotLoaded,
}

impl DuplicateFileResult {
    pub fn tab_id(self) -> Option<TabId> {
        match self {
            Self::Duplicated { tab, .. } => Some(tab),
            Self::ActivatedExisting { existing, .. } => Some(existing),
            Self::TabsDisabled | Self::SourceTabMissing | Self::NotAFile | Self::NotLoaded => None,
        }
    }

    #[cfg(test)]
    pub fn disabled_reason(self) -> Option<&'static str> {
        match self {
            Self::Duplicated { .. } | Self::ActivatedExisting { .. } => None,
            Self::TabsDisabled => Some(DUPLICATE_FILE_TABS_DISABLED_REASON),
            Self::SourceTabMissing => Some(DUPLICATE_FILE_SOURCE_MISSING_REASON),
            Self::NotAFile => Some(DUPLICATE_FILE_NOT_A_FILE_REASON),
            Self::NotLoaded => Some(DUPLICATE_FILE_NOT_LOADED_REASON),
        }
    }
}

pub const MOVE_RESOURCE_ALREADY_IN_TARGET_REASON: &str =
    "This resource is already open in the other pane.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MoveTabEligibility {
    Eligible,
    SourceTabMissing,
    AlreadyInTargetPane,
    ResourceAlreadyInTarget { existing: TabId },
}

impl MoveTabEligibility {
    pub fn disabled_reason(self) -> Option<&'static str> {
        match self {
            Self::Eligible => None,
            Self::SourceTabMissing => Some(TAB_SOURCE_MISSING_REASON),
            Self::AlreadyInTargetPane => Some(MOVE_ALREADY_IN_TARGET_PANE_REASON),
            Self::ResourceAlreadyInTarget { .. } => Some(MOVE_RESOURCE_ALREADY_IN_TARGET_REASON),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MoveTabResult {
    Moved {
        tab: TabId,
        source: PaneId,
        target: PaneId,
    },
    SourceTabMissing,
    AlreadyInTargetPane,
    ResourceAlreadyInTarget {
        existing: TabId,
    },
}

impl MoveTabResult {
    pub fn disabled_reason(self) -> Option<&'static str> {
        match self {
            Self::Moved { .. } => None,
            Self::SourceTabMissing => Some(TAB_SOURCE_MISSING_REASON),
            Self::AlreadyInTargetPane => Some(MOVE_ALREADY_IN_TARGET_PANE_REASON),
            Self::ResourceAlreadyInTarget { .. } => Some(MOVE_RESOURCE_ALREADY_IN_TARGET_REASON),
        }
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MoveTabRefusal {
    SourceTabMissing,
    AlreadyInTargetPane,
    ResourceAlreadyInTarget { existing: TabId },
}

#[cfg(test)]
impl MoveTabRefusal {
    pub fn disabled_reason(self) -> &'static str {
        match self {
            Self::SourceTabMissing => TAB_SOURCE_MISSING_REASON,
            Self::AlreadyInTargetPane => MOVE_ALREADY_IN_TARGET_PANE_REASON,
            Self::ResourceAlreadyInTarget { .. } => MOVE_RESOURCE_ALREADY_IN_TARGET_REASON,
        }
    }
}

#[cfg(test)]
impl TryFrom<MoveTabResult> for MoveTabRefusal {
    type Error = MoveTabResult;

    fn try_from(result: MoveTabResult) -> Result<Self, Self::Error> {
        match result {
            MoveTabResult::Moved { .. } => Err(result),
            MoveTabResult::SourceTabMissing => Ok(Self::SourceTabMissing),
            MoveTabResult::AlreadyInTargetPane => Ok(Self::AlreadyInTargetPane),
            MoveTabResult::ResourceAlreadyInTarget { existing } => {
                Ok(Self::ResourceAlreadyInTarget { existing })
            }
        }
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentOpenToSideResult {
    Opened {
        tab: TabId,
        pane: PaneId,
    },
    Moved {
        tab: TabId,
        source: PaneId,
        target: PaneId,
    },
    Revealed {
        tab: TabId,
        pane: PaneId,
    },
    TabsDisabled,
    CrossProject,
    NothingWouldRemain,
    MoveRefused(MoveTabRefusal),
}

#[cfg(test)]
impl AgentOpenToSideResult {
    pub fn disabled_reason(self) -> Option<&'static str> {
        match self {
            Self::Opened { .. } | Self::Moved { .. } | Self::Revealed { .. } => None,
            Self::TabsDisabled => Some(CENTER_TABS_DISABLED_REASON),
            Self::CrossProject => Some(AGENT_OPEN_TO_SIDE_CROSS_PROJECT_REASON),
            Self::NothingWouldRemain => Some(OPEN_TO_SIDE_NOTHING_WOULD_REMAIN_REASON),
            Self::MoveRefused(refusal) => Some(refusal.disabled_reason()),
        }
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffOpenToSideResult {
    Opened {
        tab: TabId,
        pane: PaneId,
    },
    Moved {
        tab: TabId,
        source: PaneId,
        target: PaneId,
    },
    Revealed {
        tab: TabId,
        pane: PaneId,
    },
    TabsDisabled,
    CrossProject,
    NothingWouldRemain,
    MoveRefused(MoveTabRefusal),
}

#[cfg(test)]
impl DiffOpenToSideResult {
    pub fn disabled_reason(self) -> Option<&'static str> {
        match self {
            Self::Opened { .. } | Self::Moved { .. } | Self::Revealed { .. } => None,
            Self::TabsDisabled => Some(CENTER_TABS_DISABLED_REASON),
            Self::CrossProject => Some(OPEN_TO_SIDE_CROSS_PROJECT_REASON),
            Self::NothingWouldRemain => Some(OPEN_TO_SIDE_NOTHING_WOULD_REMAIN_REASON),
            Self::MoveRefused(refusal) => Some(refusal.disabled_reason()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PendingOpenDestination(PaneId);

impl PendingOpenDestination {
    pub fn new(pane: PaneId) -> Self {
        Self(pane)
    }

    pub fn pane(self) -> PaneId {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PendingFileNavigation {
    Line(u32),
    Offset(u32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PendingFileOpen {
    RefreshInPlace,
    Open {
        destination: PendingOpenDestination,
        navigation: Option<PendingFileNavigation>,
    },
}

#[derive(Clone, Debug)]
pub struct PaneState {
    pub tabs: Vec<Tab>,
    pub active_tab_id: Option<TabId>,
}

impl PaneState {
    fn empty() -> Self {
        Self {
            tabs: Vec::new(),
            active_tab_id: None,
        }
    }

    fn home() -> Self {
        let id = next_tab_id();
        Self {
            tabs: vec![Tab {
                id,
                content: TabContent::Home,
                label: "Home".to_string(),
                closeable: false,
            }],
            active_tab_id: Some(id),
        }
    }

    fn from_tab(tab: Tab) -> Self {
        let id = tab.id;
        Self {
            tabs: vec![tab],
            active_tab_id: Some(id),
        }
    }

    fn activate(&mut self, id: TabId) -> bool {
        if self.tabs.iter().any(|tab| tab.id == id) {
            self.active_tab_id = Some(id);
            true
        } else {
            false
        }
    }

    fn remove_tabs(&mut self, doomed: &HashSet<TabId>) {
        let old_active = self.active_tab_id;
        let old_active_index =
            old_active.and_then(|id| self.tabs.iter().position(|tab| tab.id == id));
        self.tabs.retain(|tab| !doomed.contains(&tab.id));
        if old_active.is_none_or(|id| !self.tabs.iter().any(|tab| tab.id == id)) {
            self.active_tab_id = if self.tabs.is_empty() {
                None
            } else {
                Some(self.tabs[old_active_index.unwrap_or(0).min(self.tabs.len() - 1)].id)
            };
        }
    }
}

#[derive(Clone, Debug)]
pub enum CenterLayout {
    Single(PaneState),
    Split {
        primary: PaneState,
        secondary: PaneState,
        focused: PaneId,
        ratio: SplitRatio,
    },
}

#[derive(Clone, Debug)]
pub struct CenterZoneState {
    pub layout: CenterLayout,
}

impl CenterZoneState {
    pub fn new_home() -> Self {
        Self {
            layout: CenterLayout::Single(PaneState::home()),
        }
    }

    pub fn focused_id(&self) -> PaneId {
        match &self.layout {
            CenterLayout::Single(_) => PaneId::Primary,
            CenterLayout::Split { focused, .. } => *focused,
        }
    }

    pub fn is_split(&self) -> bool {
        matches!(self.layout, CenterLayout::Split { .. })
    }

    pub fn split_ratio(&self) -> Option<SplitRatio> {
        match &self.layout {
            CenterLayout::Single(_) => None,
            CenterLayout::Split { ratio, .. } => Some(*ratio),
        }
    }

    pub fn set_split_ratio(&mut self, value: SplitRatio) {
        if let CenterLayout::Split { ratio, .. } = &mut self.layout {
            *ratio = value;
        }
    }

    pub fn resolve(&self, target: OpenTarget) -> PaneId {
        match target {
            OpenTarget::Focused => self.focused_id(),
            OpenTarget::Beside => self.focused_id().other(),
        }
    }

    pub fn pane(&self, id: PaneId) -> Option<&PaneState> {
        match (&self.layout, id) {
            (CenterLayout::Single(primary), PaneId::Primary) => Some(primary),
            (CenterLayout::Single(_), PaneId::Secondary) => None,
            (CenterLayout::Split { primary, .. }, PaneId::Primary) => Some(primary),
            (CenterLayout::Split { secondary, .. }, PaneId::Secondary) => Some(secondary),
        }
    }

    pub fn pane_mut(&mut self, id: PaneId) -> Option<&mut PaneState> {
        match (&mut self.layout, id) {
            (CenterLayout::Single(primary), PaneId::Primary) => Some(primary),
            (CenterLayout::Single(_), PaneId::Secondary) => None,
            (CenterLayout::Split { primary, .. }, PaneId::Primary) => Some(primary),
            (CenterLayout::Split { secondary, .. }, PaneId::Secondary) => Some(secondary),
        }
    }

    pub fn focused_pane(&self) -> &PaneState {
        match &self.layout {
            CenterLayout::Single(primary) => primary,
            CenterLayout::Split {
                primary,
                secondary,
                focused,
                ..
            } => match focused {
                PaneId::Primary => primary,
                PaneId::Secondary => secondary,
            },
        }
    }

    pub fn panes(&self) -> impl Iterator<Item = (PaneId, &PaneState)> {
        [
            self.pane(PaneId::Primary)
                .map(|pane| (PaneId::Primary, pane)),
            self.pane(PaneId::Secondary)
                .map(|pane| (PaneId::Secondary, pane)),
        ]
        .into_iter()
        .flatten()
    }

    pub fn all_tabs(&self) -> impl Iterator<Item = (PaneId, &Tab)> {
        self.panes()
            .flat_map(|(pane_id, pane)| pane.tabs.iter().map(move |tab| (pane_id, tab)))
    }

    pub fn all_tab_ids(&self) -> Vec<TabId> {
        self.all_tabs().map(|(_, tab)| tab.id).collect()
    }

    #[cfg(test)]
    pub fn tabs(&self) -> &[Tab] {
        &self.focused_pane().tabs
    }

    pub fn active_tab_id(&self) -> Option<TabId> {
        self.focused_pane().active_tab_id
    }

    pub fn pane_active_tab_id(&self, pane: PaneId) -> Option<TabId> {
        self.pane(pane).and_then(|pane| pane.active_tab_id)
    }

    pub fn tab(&self, id: TabId) -> Option<&Tab> {
        self.all_tabs()
            .find_map(|(_, tab)| (tab.id == id).then_some(tab))
    }

    pub fn tab_mut(&mut self, id: TabId) -> Option<&mut Tab> {
        match &mut self.layout {
            CenterLayout::Single(primary) => primary.tabs.iter_mut().find(|tab| tab.id == id),
            CenterLayout::Split {
                primary, secondary, ..
            } => primary
                .tabs
                .iter_mut()
                .chain(secondary.tabs.iter_mut())
                .find(|tab| tab.id == id),
        }
    }

    pub fn for_each_tab_mut(&mut self, mut action: impl FnMut(PaneId, &mut Tab)) {
        match &mut self.layout {
            CenterLayout::Single(primary) => {
                for tab in &mut primary.tabs {
                    action(PaneId::Primary, tab);
                }
            }
            CenterLayout::Split {
                primary, secondary, ..
            } => {
                for tab in &mut primary.tabs {
                    action(PaneId::Primary, tab);
                }
                for tab in &mut secondary.tabs {
                    action(PaneId::Secondary, tab);
                }
            }
        }
    }

    pub fn find_tab_in(&self, pane: PaneId, content: &TabContent) -> Option<TabId> {
        self.pane(pane)?
            .tabs
            .iter()
            .find(|tab| tab.content == *content)
            .map(|tab| tab.id)
    }

    pub fn find_tab(&self, content: &TabContent) -> Option<TabId> {
        let focused = self.focused_id();
        self.find_tab_in(focused, content)
            .or_else(|| self.find_tab_in(focused.other(), content))
    }

    #[cfg(test)]
    pub fn occurrences(&self, content: &TabContent) -> Vec<(PaneId, TabId)> {
        self.all_tabs()
            .filter(|(_, tab)| tab.content == *content)
            .map(|(pane, tab)| (pane, tab.id))
            .collect()
    }

    pub fn locate_tab(&self, id: TabId) -> Option<PaneId> {
        self.panes()
            .find_map(|(pane, state)| state.tabs.iter().any(|tab| tab.id == id).then_some(pane))
    }

    pub fn open(&mut self, content: TabContent, label: String, closeable: bool) -> TabId {
        if let Some(id) = self.find_tab(&content) {
            self.activate(id);
            return id;
        }
        let target = self.focused_id();
        self.open_in(target, content, label, closeable, SplitRatio::default())
    }

    pub fn open_in(
        &mut self,
        target: PaneId,
        content: TabContent,
        label: String,
        closeable: bool,
        ratio: SplitRatio,
    ) -> TabId {
        if let Some(id) = self.find_tab_in(target, &content) {
            self.activate(id);
            return id;
        }
        if !matches!(&content, TabContent::File { .. })
            && let Some(id) = self.find_tab(&content)
        {
            self.activate(id);
            return id;
        }
        let id = next_tab_id();
        let tab = Tab {
            id,
            content,
            label,
            closeable,
        };
        match (&mut self.layout, target) {
            (CenterLayout::Single(primary), PaneId::Primary) => {
                primary.tabs.push(tab);
                primary.active_tab_id = Some(id);
            }
            (CenterLayout::Single(_), PaneId::Secondary) => {
                let old_layout =
                    std::mem::replace(&mut self.layout, CenterLayout::Single(PaneState::empty()));
                let primary = match old_layout {
                    CenterLayout::Single(primary) => primary,
                    other => {
                        self.layout = other;
                        return id;
                    }
                };
                self.layout = CenterLayout::Split {
                    primary,
                    secondary: PaneState::from_tab(tab),
                    focused: PaneId::Secondary,
                    ratio,
                };
            }
            (
                CenterLayout::Split {
                    primary, focused, ..
                },
                PaneId::Primary,
            ) => {
                primary.tabs.push(tab);
                primary.active_tab_id = Some(id);
                *focused = PaneId::Primary;
            }
            (
                CenterLayout::Split {
                    secondary, focused, ..
                },
                PaneId::Secondary,
            ) => {
                secondary.tabs.push(tab);
                secondary.active_tab_id = Some(id);
                *focused = PaneId::Secondary;
            }
        }
        id
    }

    pub fn reveal_tab(&mut self, id: TabId) -> bool {
        let Some(pane_id) = self.locate_tab(id) else {
            return false;
        };
        if !self.set_active_tab_in_pane(pane_id, id) {
            return false;
        }
        if let CenterLayout::Split { focused, .. } = &mut self.layout {
            *focused = pane_id;
        }
        true
    }

    pub fn set_active_tab_in_pane(&mut self, pane: PaneId, id: TabId) -> bool {
        self.pane_mut(pane)
            .is_some_and(|pane_state| pane_state.activate(id))
    }

    pub fn activate(&mut self, id: TabId) {
        self.reveal_tab(id);
    }

    pub fn update_tab(&mut self, id: TabId, content: TabContent, label: String) -> bool {
        let Some(tab) = self.tab_mut(id) else {
            return false;
        };
        tab.content = content;
        tab.label = label;
        true
    }

    pub fn close(&mut self, id: TabId) {
        let Some(tab) = self.tab(id) else {
            return;
        };
        if !tab.closeable {
            return;
        }
        self.remove_tabs(&HashSet::from([id]));
    }

    pub fn replace_active(&mut self, content: TabContent, label: String, closeable: bool) -> TabId {
        if let Some(active_id) = self.active_tab_id()
            && let Some(tab) = self.tab_mut(active_id)
        {
            tab.content = content;
            tab.label = label;
            tab.closeable = closeable;
            return active_id;
        }
        self.open(content, label, closeable)
    }

    pub fn active_tab(&self) -> Option<&Tab> {
        self.active_tab_id().and_then(|id| self.tab(id))
    }

    pub fn active_content(&self) -> Option<&TabContent> {
        self.active_tab().map(|t| &t.content)
    }

    #[cfg(test)]
    pub fn close_others(&mut self, id: TabId) {
        let Some(pane_id) = self.locate_tab(id) else {
            return;
        };
        let doomed: HashSet<TabId> = self
            .pane(pane_id)
            .into_iter()
            .flat_map(|pane| pane.tabs.iter())
            .filter(|tab| tab.id != id && tab.closeable)
            .map(|tab| tab.id)
            .collect();
        self.remove_tabs(&doomed);
        self.activate(id);
    }

    #[cfg(test)]
    pub fn close_to_right(&mut self, id: TabId) {
        let Some(pane_id) = self.locate_tab(id) else {
            return;
        };
        let Some(pane) = self.pane(pane_id) else {
            return;
        };
        let Some(index) = pane.tabs.iter().position(|tab| tab.id == id) else {
            return;
        };
        let doomed = pane.tabs[index + 1..]
            .iter()
            .filter(|tab| tab.closeable)
            .map(|tab| tab.id)
            .collect();
        self.remove_tabs(&doomed);
    }

    #[cfg(test)]
    pub fn close_all(&mut self) {
        let doomed = self
            .all_tabs()
            .filter(|(_, tab)| tab.closeable)
            .map(|(_, tab)| tab.id)
            .collect();
        self.remove_tabs(&doomed);
    }

    pub fn rename_tab_label(&mut self, id: TabId, new_label: String) {
        if let Some(tab) = self.tab_mut(id) {
            tab.label = new_label;
        }
    }

    pub fn composer_owner(&self) -> Option<(PaneId, TabId)> {
        let focused = self.focused_id();
        [focused, focused.other()].into_iter().find_map(|pane_id| {
            let pane = self.pane(pane_id)?;
            let tab_id = pane.active_tab_id?;
            let tab = pane.tabs.iter().find(|tab| tab.id == tab_id)?;
            matches!(&tab.content, TabContent::Chat { .. }).then_some((pane_id, tab_id))
        })
    }

    pub fn duplicate_file_to(
        &mut self,
        source: TabId,
        target: PaneId,
        ratio: SplitRatio,
    ) -> Option<TabId> {
        let tab = self.tab(source)?.clone();
        if !matches!(&tab.content, TabContent::File { .. }) {
            return None;
        }
        if let Some(existing) = self.find_tab_in(target, &tab.content) {
            self.activate(existing);
            return Some(existing);
        }
        Some(self.open_in(target, tab.content, tab.label, tab.closeable, ratio))
    }

    pub fn move_tab_eligibility(&self, target: PaneId, id: TabId) -> MoveTabEligibility {
        let Some(source) = self.locate_tab(id) else {
            return MoveTabEligibility::SourceTabMissing;
        };
        if source == target {
            return MoveTabEligibility::AlreadyInTargetPane;
        }
        let Some(tab) = self.tab(id) else {
            return MoveTabEligibility::SourceTabMissing;
        };
        if let Some(existing) = self.find_tab_in(target, &tab.content) {
            return MoveTabEligibility::ResourceAlreadyInTarget { existing };
        }
        MoveTabEligibility::Eligible
    }

    pub fn move_tab_to(&mut self, target: PaneId, id: TabId, ratio: SplitRatio) -> MoveTabResult {
        match self.move_tab_eligibility(target, id) {
            MoveTabEligibility::Eligible => {}
            MoveTabEligibility::SourceTabMissing => return MoveTabResult::SourceTabMissing,
            MoveTabEligibility::AlreadyInTargetPane => {
                return MoveTabResult::AlreadyInTargetPane;
            }
            MoveTabEligibility::ResourceAlreadyInTarget { existing } => {
                return MoveTabResult::ResourceAlreadyInTarget { existing };
            }
        }
        let Some(source) = self.locate_tab(id) else {
            return MoveTabResult::SourceTabMissing;
        };
        let Some(tab) = self.tab(id).cloned() else {
            return MoveTabResult::SourceTabMissing;
        };
        let mut doomed = HashSet::new();
        doomed.insert(id);
        if let Some(source_pane) = self.pane_mut(source) {
            source_pane.remove_tabs(&doomed);
        }
        match (&mut self.layout, target) {
            (CenterLayout::Single(primary), PaneId::Primary) => {
                primary.tabs.push(tab);
                primary.active_tab_id = Some(id);
            }
            (CenterLayout::Single(_), PaneId::Secondary) => {
                let old_layout =
                    std::mem::replace(&mut self.layout, CenterLayout::Single(PaneState::empty()));
                let primary = match old_layout {
                    CenterLayout::Single(primary) => primary,
                    other => {
                        self.layout = other;
                        return MoveTabResult::SourceTabMissing;
                    }
                };
                self.layout = CenterLayout::Split {
                    primary,
                    secondary: PaneState::from_tab(tab),
                    focused: PaneId::Secondary,
                    ratio,
                };
            }
            (
                CenterLayout::Split {
                    primary, focused, ..
                },
                PaneId::Primary,
            ) => {
                primary.tabs.push(tab);
                primary.active_tab_id = Some(id);
                *focused = PaneId::Primary;
            }
            (
                CenterLayout::Split {
                    secondary, focused, ..
                },
                PaneId::Secondary,
            ) => {
                secondary.tabs.push(tab);
                secondary.active_tab_id = Some(id);
                *focused = PaneId::Secondary;
            }
        }
        self.collapse_empty_pane();
        MoveTabResult::Moved {
            tab: id,
            source,
            target,
        }
    }

    pub fn split_tab_to(&mut self, target: PaneId, id: TabId, ratio: SplitRatio) -> MoveTabResult {
        let CenterLayout::Single(primary) = &self.layout else {
            return self.move_tab_to(target, id, ratio);
        };
        if !primary.tabs.iter().any(|tab| tab.id == id) {
            return MoveTabResult::SourceTabMissing;
        }

        let old_layout =
            std::mem::replace(&mut self.layout, CenterLayout::Single(PaneState::empty()));
        let CenterLayout::Single(mut remaining) = old_layout else {
            unreachable!("the layout was checked as single above");
        };
        let Some(tab) = remaining.tabs.iter().find(|tab| tab.id == id).cloned() else {
            self.layout = CenterLayout::Single(remaining);
            return MoveTabResult::SourceTabMissing;
        };
        let mut moved = HashSet::new();
        moved.insert(id);
        remaining.remove_tabs(&moved);

        let dragged = PaneState::from_tab(tab);
        let (primary, secondary) = match target {
            PaneId::Primary => (dragged, remaining),
            PaneId::Secondary => (remaining, dragged),
        };
        self.layout = CenterLayout::Split {
            primary,
            secondary,
            focused: target,
            ratio,
        };
        self.collapse_empty_pane();
        MoveTabResult::Moved {
            tab: id,
            source: PaneId::Primary,
            target,
        }
    }

    pub fn remove_tabs(&mut self, doomed: &HashSet<TabId>) {
        match &mut self.layout {
            CenterLayout::Single(primary) => primary.remove_tabs(doomed),
            CenterLayout::Split {
                primary, secondary, ..
            } => {
                primary.remove_tabs(doomed);
                secondary.remove_tabs(doomed);
            }
        }
        self.collapse_empty_pane();
    }

    fn collapse_empty_pane(&mut self) {
        let replacement = match &self.layout {
            CenterLayout::Single(primary) if primary.tabs.is_empty() => Some(PaneState::home()),
            CenterLayout::Split {
                primary, secondary, ..
            } if primary.tabs.is_empty() && secondary.tabs.is_empty() => Some(PaneState::home()),
            CenterLayout::Split {
                primary, secondary, ..
            } if primary.tabs.is_empty() => Some(secondary.clone()),
            CenterLayout::Split {
                primary, secondary, ..
            } if secondary.tabs.is_empty() => Some(primary.clone()),
            _ => None,
        };
        if let Some(pane) = replacement {
            self.layout = CenterLayout::Single(pane);
        }
    }
}

#[cfg(test)]
fn agent_open_to_side_block_for(
    tabs_enabled: bool,
    active_project: Option<&ActiveProjectRef>,
    center_zone: &CenterZoneState,
    agent_ref: &ActiveAgentRef,
    project: Option<&ActiveProjectRef>,
) -> Option<AgentOpenToSideResult> {
    if !tabs_enabled {
        return Some(AgentOpenToSideResult::TabsDisabled);
    }
    if active_project != project
        || project.is_some_and(|project| project.host_id != agent_ref.host_id)
    {
        return Some(AgentOpenToSideResult::CrossProject);
    }

    let content = TabContent::chat_with_agent(agent_ref.clone());
    let focused = center_zone.focused_id();
    let target = focused.other();
    let (pane, tab) = center_zone
        .find_tab(&content)
        .and_then(|tab| center_zone.locate_tab(tab).map(|pane| (pane, tab)))?;
    if pane == target {
        return None;
    }
    let source_retains_content = center_zone
        .pane(pane)
        .is_some_and(|source| source.tabs.iter().any(|candidate| candidate.id != tab));
    if !source_retains_content {
        return Some(AgentOpenToSideResult::NothingWouldRemain);
    }
    match center_zone.move_tab_eligibility(target, tab) {
        MoveTabEligibility::Eligible => None,
        MoveTabEligibility::SourceTabMissing => Some(AgentOpenToSideResult::MoveRefused(
            MoveTabRefusal::SourceTabMissing,
        )),
        MoveTabEligibility::AlreadyInTargetPane => Some(AgentOpenToSideResult::MoveRefused(
            MoveTabRefusal::AlreadyInTargetPane,
        )),
        MoveTabEligibility::ResourceAlreadyInTarget { existing } => {
            Some(AgentOpenToSideResult::MoveRefused(
                MoveTabRefusal::ResourceAlreadyInTarget { existing },
            ))
        }
    }
}

#[cfg(test)]
fn diff_open_to_side_block_for(
    tabs_enabled: bool,
    active_project: Option<&ActiveProjectRef>,
    center_zone: &CenterZoneState,
    key: &DiffKey,
) -> Option<DiffOpenToSideResult> {
    if !tabs_enabled {
        return Some(DiffOpenToSideResult::TabsDisabled);
    }
    if !active_project.is_some_and(|project| {
        project.host_id == key.host_id && project.project_id == key.project_id
    }) {
        return Some(DiffOpenToSideResult::CrossProject);
    }

    let content = key.tab_content();
    let focused = center_zone.focused_id();
    let target = focused.other();
    let (pane, tab) = center_zone
        .find_tab(&content)
        .and_then(|tab| center_zone.locate_tab(tab).map(|pane| (pane, tab)))?;
    if pane == target {
        return None;
    }
    let source_retains_content = center_zone
        .pane(pane)
        .is_some_and(|source| source.tabs.iter().any(|candidate| candidate.id != tab));
    if !source_retains_content {
        return Some(DiffOpenToSideResult::NothingWouldRemain);
    }
    match center_zone.move_tab_eligibility(target, tab) {
        MoveTabEligibility::Eligible => None,
        MoveTabEligibility::SourceTabMissing => Some(DiffOpenToSideResult::MoveRefused(
            MoveTabRefusal::SourceTabMissing,
        )),
        MoveTabEligibility::AlreadyInTargetPane => Some(DiffOpenToSideResult::MoveRefused(
            MoveTabRefusal::AlreadyInTargetPane,
        )),
        MoveTabEligibility::ResourceAlreadyInTarget { existing } => Some(
            DiffOpenToSideResult::MoveRefused(MoveTabRefusal::ResourceAlreadyInTarget { existing }),
        ),
    }
}

fn duplicate_file_eligibility_for(
    tabs_enabled: bool,
    center_zone: &CenterZoneState,
    open_files: &HashMap<FileResourceKey, OpenFile>,
    target: PaneId,
    source: TabId,
) -> DuplicateFileEligibility {
    if !tabs_enabled {
        return DuplicateFileEligibility::TabsDisabled;
    }
    let Some(tab) = center_zone.tab(source) else {
        return DuplicateFileEligibility::SourceTabMissing;
    };
    let TabContent::File { key } = &tab.content else {
        return DuplicateFileEligibility::NotAFile;
    };
    if !open_files.contains_key(key) {
        return DuplicateFileEligibility::NotLoaded;
    }
    if let Some(existing) = center_zone.find_tab_in(target, &tab.content) {
        return DuplicateFileEligibility::TargetAlreadyContainsResource { existing };
    }
    DuplicateFileEligibility::Enabled
}

impl Default for CenterZoneState {
    fn default() -> Self {
        Self::new_home()
    }
}

// ── Dock ────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
pub enum DockVisibility {
    Visible,
    Hidden,
}

/// Which tab of the left dock is currently shown. Stored in `AppState` (rather
/// than locally in the dock component) so a keyboard shortcut and the
/// "search in folder" file-explorer action can switch to the Search tab.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeftTab {
    Files,
    Git,
    Search,
    /// Find-references results panel (M5). Auto-activated when a Shift+F12
    /// find-references query runs.
    References,
}

/// Which tab of the right dock is currently shown. Stored in `AppState` so
/// global UI actions such as command-palette entries can open a specific panel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RightTab {
    Agents,
    Sessions,
    Teams,
    Workflows,
}

/// All persistent state for the project-wide search panel. Lives in `AppState`
/// so streamed results survive the panel being display-toggled (or its dock
/// being hidden) and so `dispatch` can append incoming result frames.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProjectSearchUiState {
    pub query: String,
    pub case_sensitive: bool,
    pub whole_word: bool,
    pub use_regex: bool,
    pub include_ignored: bool,
    /// When set, the search is scoped to this root-relative folder prefix
    /// (driven by the "search in folder" action).
    pub path_prefix: Option<String>,
    /// When non-empty, only these roots are searched (paired with
    /// `path_prefix` for "search in folder").
    pub roots: Vec<ProjectRootPath>,
    /// The `search_id` of the most recently issued search. Incoming result /
    /// complete frames are ignored unless they carry this id.
    pub active_search_id: u64,
    /// True between issuing a search and receiving its `complete` frame.
    pub in_flight: bool,
    /// One entry per matching file, in arrival order.
    pub results: Vec<ProjectSearchFileResult>,
    pub total_files: u32,
    pub total_matches: u32,
    pub truncated: bool,
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ProjectReferencesMode {
    #[default]
    References,
    DefinitionTargets,
}

/// All persistent state for the find-references results panel (M5). Lives in
/// `AppState` so streamed results survive the panel being display-toggled and so
/// `dispatch` can append incoming `code_intel_references_results` frames. Mirrors
/// [`ProjectSearchUiState`], correlated by a `references_id` domain id and the
/// exact initiating occurrence/resource so late frames cannot be reconstructed
/// against whichever project or pane happens to be active at response time.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProjectReferencesUiState {
    pub mode: ProjectReferencesMode,
    /// The exact file occurrence that initiated this query or definition
    /// chooser. Result routing can prefer this occurrence without consulting
    /// response-time focus; the resource/version remain the request identity.
    pub source_tab: Option<TabId>,
    pub source_key: Option<FileResourceKey>,
    pub source_version: Option<ProjectFileVersion>,
    /// The `references_id` of the most recently issued query. Incoming result /
    /// complete frames are ignored unless they carry this id and match the stored
    /// source resource context.
    pub active_references_id: u64,
    /// True between issuing a query and its terminal `complete` frame.
    pub in_flight: bool,
    /// The identifier the query is about, for the panel header. `None` when the
    /// symbol text wasn't captured.
    pub symbol: Option<String>,
    /// One entry per matching file, in arrival order.
    pub results: Vec<CodeIntelReferencesFileResult>,
    /// For `DefinitionTargets` mode, one target per rendered result row in
    /// flattened file/line order. References mode leaves this empty and rows
    /// navigate by line as before.
    pub row_targets: Vec<CodeIntelLocation>,
    pub total_files: u32,
    pub total_references: u32,
    pub truncated: bool,
    pub cancelled: bool,
    pub error: Option<String>,
}

impl ProjectReferencesUiState {
    pub fn source(&self) -> Option<(TabId, &FileResourceKey, ProjectFileVersion)> {
        Some((
            self.source_tab?,
            self.source_key.as_ref()?,
            self.source_version?,
        ))
    }
}

#[derive(Clone, Debug)]
pub struct ChatMessageEntry {
    pub message: ChatMessage,
    pub tool_requests: Vec<ToolRequestEntry>,
}

#[derive(Clone, Debug)]
pub struct ToolRequestEntry {
    pub request: ToolRequest,
    pub result: Option<ToolExecutionCompletedData>,
}

#[derive(Clone, Debug)]
pub struct StreamingToolRequest {
    pub tool_call_id: String,
    pub entry: ArcRwSignal<ToolRequestEntry>,
}

// ── Chat transcript rows ────────────────────────────────────────────────

thread_local! {
    static NEXT_CHAT_ROW_ID: Cell<u64> = const { Cell::new(0) };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ChatRowId(pub u64);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ToolCallId(pub String);

fn next_chat_row_id() -> ChatRowId {
    NEXT_CHAT_ROW_ID.with(|cell| {
        let id = cell.get();
        cell.set(id + 1);
        ChatRowId(id)
    })
}

#[derive(Clone, Debug)]
pub struct ChatRowHandle {
    pub id: ChatRowId,
    pub entry: ArcRwSignal<ChatMessageEntry>,
}

impl ChatRowHandle {
    pub fn new(entry: ChatMessageEntry) -> Self {
        Self {
            id: next_chat_row_id(),
            entry: ArcRwSignal::new(entry),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionHistoryState {
    pub message_count: u32,
    pub oldest_seq: Option<u64>,
    pub has_more_before: bool,
    pub loading: bool,
}

#[derive(Clone, Debug)]
pub struct OpenFile {
    pub path: ProjectPath,
    /// Version of these contents, from the project-stream actor's centralized
    /// counter. Code-intel frames apply only when their version equals this.
    pub version: ProjectFileVersion,
    pub contents: Option<String>,
    pub is_binary: bool,
    /// Server-reported: the file no longer exists on disk (a refresh read
    /// answered `missing`). `contents` keeps the last-seen text so the viewer
    /// can label it "deleted" instead of going blank; cleared by the next
    /// contents frame after the file is re-created.
    pub missing: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FileFocus {
    pub tab: TabId,
    pub key: FileResourceKey,
    pub version: ProjectFileVersion,
}

/// Key for the code-intelligence signal. Carries the explicit owning
/// `(host_id, project_id)` plus the file path, so two projects/hosts that share
/// the same root-path string can't collide. The `ProjectFileVersion` is tracked
/// *inside* [`CodeIntelFileState`] (the version-equals-rendered rule), not in
/// the key.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CodeIntelKey {
    pub host_id: String,
    pub project_id: ProjectId,
    pub path: ProjectPath,
}

/// The semantic data the server pushed for one file version.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CodeIntelData {
    pub status: Option<CodeIntelStatusPayload>,
    pub model: Option<CodeIntelFileModelPayload>,
    pub error: Option<CodeIntelErrorPayload>,
    /// Latest full-file diagnostics snapshot for this version. A
    /// `code_intel_diagnostics` frame **replaces** this set wholesale (spec
    /// §4.2) — diagnostics are not merged like the definition model.
    pub diagnostics: Vec<CodeIntelDiagnostic>,
}

impl CodeIntelData {
    /// Merge an incoming file model into the existing one for the same `(path,
    /// version)`. The server delivers the whole-file model **incrementally**
    /// (spec §2.1/§4.2): the first frame carries occurrence ranges, later frames
    /// at the same version fill in `definition` targets per occurrence. So
    /// occurrences are merged **by range**, and within a matching range the
    /// `definition` targets are **unioned** (deduped) rather than overwritten —
    /// a later frame that re-sends a range with an empty/partial `definition`
    /// must never wipe a target that an earlier frame already resolved. This is
    /// what makes the streamed go-to-definition map (M3) converge instead of
    /// flapping. The latest frame's `completeness` / `model_range` / `provider`
    /// / `language` win; `role` takes the latest, `display` the latest non-empty.
    pub fn merge_model(&mut self, incoming: CodeIntelFileModelPayload) {
        match self.model.as_mut() {
            None => self.model = Some(incoming),
            Some(existing) => {
                for occurrence in incoming.occurrences {
                    match existing
                        .occurrences
                        .iter_mut()
                        .find(|candidate| candidate.range == occurrence.range)
                    {
                        Some(slot) => merge_occurrence(slot, occurrence),
                        None => existing.occurrences.push(occurrence),
                    }
                }
                existing.completeness = incoming.completeness;
                existing.model_range = incoming.model_range;
                existing.provider = incoming.provider;
                existing.language = incoming.language;
                existing.version = incoming.version;
            }
        }
    }
}

/// Merge an incoming occurrence into an existing one with the same range.
/// `definition` targets are unioned (deduped) so already-resolved targets
/// survive a later frame that re-sends the range with an empty/partial set;
/// `role` takes the latest value and `display` the latest non-empty value.
fn merge_occurrence(slot: &mut CodeIntelOccurrence, incoming: CodeIntelOccurrence) {
    for location in incoming.definition {
        if !slot.definition.contains(&location) {
            slot.definition.push(location);
        }
    }
    slot.role = incoming.role;
    if !incoming.display.is_empty() {
        slot.display = incoming.display;
    }
}

/// Per-file code-intelligence state, implementing the version-equals-rendered
/// rule (`dev-docs/24-code-intelligence.md` §6): a frame is *applied* only when
/// its version equals the version of the file contents currently rendered; a
/// *newer* frame is *stashed* until the matching contents arrive; an *older*
/// frame is *dropped*.
///
/// The data is held in `by_version` (the "keyed by version" dimension); the
/// applied data is `by_version[rendered_version]`. This unifies apply and stash
/// into a single insert and makes both stale-drop directions fall out of the
/// `rendered_version` bookkeeping.
const CODE_INTEL_PRE_CONTENT_STASH_LIMIT: usize = 8;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct CodeIntelFileState {
    /// Version of the file contents currently rendered (from
    /// `ProjectFileContents`). `None` until the first contents arrive.
    pub rendered_version: Option<ProjectFileVersion>,
    pub by_version: std::collections::BTreeMap<ProjectFileVersion, CodeIntelData>,
}

impl CodeIntelFileState {
    /// Merge a versioned code-intel frame, honoring apply / stash / drop.
    /// `apply` mutates the [`CodeIntelData`] for that version. A frame older
    /// than the rendered version is dropped (it would paint over newer text).
    pub fn merge_versioned(
        &mut self,
        version: ProjectFileVersion,
        apply: impl FnOnce(&mut CodeIntelData),
    ) {
        if let Some(rendered) = self.rendered_version
            && version < rendered
        {
            // Older than what's on screen: drop.
            return;
        }
        // Equal (apply) or newer (stash): both merge into `by_version`.
        apply(self.by_version.entry(version).or_default());
        if self.rendered_version.is_none() {
            while self.by_version.len() > CODE_INTEL_PRE_CONTENT_STASH_LIMIT {
                self.by_version.pop_first();
            }
        }
    }

    /// Record that file contents at `version` are now rendered. Drops any
    /// stashed data older than `version` (it can never be shown again), which
    /// promotes the matching-version data to "applied".
    pub fn set_rendered_version(&mut self, version: ProjectFileVersion) {
        self.rendered_version = Some(version);
        self.by_version.retain(|candidate, _| *candidate >= version);
    }

    /// The data to render right now: the entry matching the rendered version,
    /// or `None` if contents haven't arrived or no frame matches yet.
    pub fn applied(&self) -> Option<&CodeIntelData> {
        self.by_version.get(&self.rendered_version?)
    }

    pub fn resolved_definition_at(
        &self,
        version: ProjectFileVersion,
        offset: u32,
    ) -> Option<(ByteRange, CodeIntelLocation)> {
        if self.rendered_version != Some(version) {
            return None;
        }
        let model = self.applied()?.model.as_ref()?;
        let occurrence = model
            .occurrences
            .iter()
            .find(|occ| occ.range.start <= offset && offset < occ.range.end)?;
        let location = occurrence.definition.first()?.clone();
        Some((occurrence.range, location))
    }

    pub fn navigable_range_at(
        &self,
        version: ProjectFileVersion,
        offset: u32,
    ) -> Option<ByteRange> {
        self.resolved_definition_at(version, offset)
            .map(|(range, _)| range)
    }

    /// The diagnostics whose range contains `offset`, at the rendered version.
    /// Used to merge diagnostic messages into the hover popover: the squiggle
    /// itself carries no readable message, so hovering the flagged span is the
    /// one place the user can read what is wrong. Zero-width ranges (some
    /// servers anchor a diagnostic to a single position) match their anchor
    /// offset. Order: most severe first, so the error reads before its hints.
    pub fn diagnostics_at(
        &self,
        version: ProjectFileVersion,
        offset: u32,
    ) -> Vec<CodeIntelDiagnostic> {
        if self.rendered_version != Some(version) {
            return Vec::new();
        }
        let Some(data) = self.applied() else {
            return Vec::new();
        };
        let mut hits: Vec<CodeIntelDiagnostic> = data
            .diagnostics
            .iter()
            .filter(|diagnostic| {
                let range = diagnostic.range;
                (range.start <= offset && offset < range.end)
                    || (range.start == range.end && offset == range.start)
            })
            .cloned()
            .collect();
        hits.sort_by_key(|diagnostic| severity_sort_rank(diagnostic.severity));
        hits
    }
}

/// Ascending sort rank: most severe first.
fn severity_sort_rank(severity: protocol::CodeIntelSeverity) -> u8 {
    match severity {
        protocol::CodeIntelSeverity::Error => 0,
        protocol::CodeIntelSeverity::Warning => 1,
        protocol::CodeIntelSeverity::Information => 2,
        protocol::CodeIntelSeverity::Hint => 3,
    }
}

/// Context for the most recent on-demand go-to-definition request (M2), stored
/// when the `code_intel_navigate` frame is sent. A `code_intel_navigate_result`
/// is only acted on when it still matches this whole context — same
/// `navigate_id`, same owning host/project, and the source file still open at
/// the same rendered version — so a result that arrives after the tab closed,
/// the file changed, or the user switched projects is dropped instead of
/// yanking the user somewhere unexpected.
#[derive(Clone, Debug, PartialEq)]
pub struct CodeIntelNavigateContext {
    pub navigate_id: u64,
    pub tab: TabId,
    pub key: FileResourceKey,
    pub version: ProjectFileVersion,
}

/// On-demand hover popover state (M2). The anchor is captured (in viewport
/// coordinates) when the hover request fires, so the popover can be positioned
/// over the hovered span the moment the correlated `code_intel_hover_result`
/// arrives. `contents` is `None` while the request is in flight — the popover
/// renders nothing until real markdown lands (no empty flash).
#[derive(Clone, Debug, PartialEq)]
pub struct HoverPopover {
    pub hover_id: u64,
    /// The exact occurrence that owns the anchor and request. A late response
    /// cannot attach to another pane's occurrence of the same file.
    pub tab: TabId,
    pub key: FileResourceKey,
    pub version: ProjectFileVersion,
    /// Absolute file byte offset the hover targets. Used to dedupe rapid
    /// mousemoves over the same identifier so the popover doesn't flicker.
    pub offset: u32,
    /// Left edge of the hovered span, viewport-relative px.
    pub anchor_left: f64,
    /// Top edge of the hovered span, viewport-relative px.
    pub anchor_top: f64,
    /// Bottom edge of the hovered span, viewport-relative px.
    pub anchor_bottom: f64,
    /// Rendered markdown, or `None` until the result arrives.
    pub contents: Option<String>,
}

/// A transient, per-tab code-intelligence notice (e.g. "definition is outside
/// the project"). Rendered as a small banner by the owning file view, which
/// also owns the auto-clear timeout. Distinct from `CodeIntelData.error`:
/// notices are informational one-shots, never a provider failure state.
#[derive(Clone, Debug, PartialEq)]
pub struct CodeIntelNotice {
    pub tab: TabId,
    pub message: String,
}

/// Cache key for `diff_contents`. Carries the explicit owning `(host_id,
/// project_id)` in addition to `(root, scope, path)` so two projects/hosts
/// that share the same root path string can't overwrite each other's diff —
/// the rendered diff body always belongs to the tab's project. `path` is the
/// file path, or empty for the whole-root (all-uncommitted) review surface.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DiffKey {
    pub host_id: String,
    pub project_id: ProjectId,
    pub root: ProjectRootPath,
    pub scope: ProjectDiffScope,
    pub path: String,
}

impl DiffKey {
    pub fn new(
        host_id: impl Into<String>,
        project_id: ProjectId,
        root: ProjectRootPath,
        scope: ProjectDiffScope,
        path: impl Into<String>,
    ) -> Self {
        Self {
            host_id: host_id.into(),
            project_id,
            root,
            scope,
            path: path.into(),
        }
    }

    #[cfg(test)]
    fn tab_content(&self) -> TabContent {
        TabContent::Diff {
            host_id: self.host_id.clone(),
            project_id: self.project_id.clone(),
            root: self.root.clone(),
            scope: self.scope,
            path: self.path.clone(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct DiffViewState {
    pub root: ProjectRootPath,
    pub scope: ProjectDiffScope,
    pub path: Option<String>,
    /// The context mode of the most recent *request* (not response). The
    /// reactive re-request effect compares this to `AppState::diff_context_mode`
    /// to decide whether to dispatch a new read, and the dispatch reducer
    /// compares `payload.context_mode` to this to reject stale responses.
    pub context_mode: DiffContextMode,
    /// True between the time a `ProjectReadDiff` is dispatched and a matching
    /// response arrives. The renderer shows a loading state when `pending` is
    /// set so stale data doesn't sit on screen while a fresh request is in
    /// flight.
    pub pending: bool,
    pub files: Vec<ProjectGitDiffFile>,
}

impl DiffViewState {
    /// Build the state to store when dispatching a fresh `ProjectReadDiff`.
    /// If the previous entry is for the same `context_mode`, its `files` are
    /// preserved to avoid flicker while refreshing. On a mode change, `files`
    /// is cleared so stale data is not visible.
    pub fn for_request(
        previous: Option<&DiffViewState>,
        root: ProjectRootPath,
        scope: ProjectDiffScope,
        path: Option<String>,
        context_mode: DiffContextMode,
    ) -> DiffViewState {
        let files = previous
            .filter(|p| p.context_mode == context_mode)
            .map(|p| p.files.clone())
            .unwrap_or_default();
        DiffViewState {
            root,
            scope,
            path,
            context_mode,
            pending: true,
            files,
        }
    }
}

/// Pure reducer for `ProjectGitDiff` responses. Returns `Some(new_state)` if
/// the payload should replace the stored entry, or `None` if it should be
/// ignored as stale.
///
/// A response is considered valid only when a matching request is still the
/// latest one in flight — i.e. when `current.context_mode ==
/// payload.context_mode`. If no entry exists (response without an outstanding
/// request), the payload is ignored.
pub fn reduce_diff_response(
    current: Option<&DiffViewState>,
    payload: ProjectGitDiffPayload,
) -> Option<DiffViewState> {
    let current = current?;
    if current.context_mode != payload.context_mode {
        return None;
    }
    Some(DiffViewState {
        root: payload.root,
        scope: payload.scope,
        path: payload.path,
        context_mode: payload.context_mode,
        pending: false,
        files: payload.files,
    })
}

#[derive(Clone, Debug)]
pub struct StreamingState {
    pub agent_name: String,
    pub model: Option<String>,
    pub text: ArcRwSignal<String>,
    pub reasoning: ArcRwSignal<String>,
    pub tool_requests: ArcRwSignal<Vec<StreamingToolRequest>>,
}

#[derive(Clone, Debug)]
pub struct TerminalInfo {
    pub host_id: String,
    pub terminal_id: TerminalId,
    pub stream: StreamPath,
    pub project_id: Option<ProjectId>,
    pub root: Option<ProjectRootPath>,
    pub cwd: String,
    pub shell: String,
    pub cols: u16,
    pub rows: u16,
    pub created_at_ms: u64,
    /// Output chunks that arrived before the xterm widget mounted. Drained by
    /// the terminal view on first mount; not used afterwards.
    pub pending_output: Vec<String>,
    /// True once an xterm instance has been created for this terminal. Output
    /// is written directly through the JS bridge from then on.
    pub widget_mounted: bool,
    pub exited: bool,
    pub exit_code: Option<i32>,
    pub exit_signal: Option<String>,
}

#[derive(Clone, Debug)]
pub enum TransientEvent {
    OperationCancelled {
        message: String,
    },
    RetryAttempt {
        attempt: u64,
        max_retries: u64,
        error: String,
        backoff_ms: u64,
    },
}

/// One entry in an agent's Tycode orchestration log, in chronological order.
/// The log stores the typed events the server emitted verbatim, plus a
/// locally-injected [`OrchestrationRecord::Cancelled`] marker for turn
/// cancellations. The orchestration panel folds this log into a presentation
/// tree at render time (see `components::orchestration_view`) — no aggregated
/// state is cached; the events are the source of truth.
#[derive(Clone, Debug)]
pub enum OrchestrationRecord {
    Event(protocol::OrchestrationEvent),
    /// A `ChatEvent::OperationCancelled` at this point in the stream. Tycode
    /// drops any in-flight fan-out/worker/sub-agent without terminal events on
    /// cancel, so the fold closes everything still running at this marker
    /// instead of leaving it stuck "running".
    Cancelled,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProjectInfo {
    pub host_id: String,
    pub project: Project,
}

pub fn root_display_name(root: &ProjectRootPath) -> String {
    display_path_name(&root.0)
}

pub fn display_path_name(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    normalized
        .trim_end_matches('/')
        .rsplit('/')
        .find(|segment| !segment.is_empty())
        .unwrap_or(path)
        .to_owned()
}

pub fn sort_project_infos(projects: &mut [ProjectInfo]) {
    projects.sort_by(|left, right| {
        left.host_id
            .cmp(&right.host_id)
            .then(left.project.sort_order.cmp(&right.project.sort_order))
            .then(left.project.name.cmp(&right.project.name))
            .then(left.project.id.0.cmp(&right.project.id.0))
    });
}

/// Close every Chat tab in `center_zone` whose `agent_ref` points at
/// `(host_id, agent_id)`. Mirror of `dispatch::close_agent_tabs`, kept
/// in `state` so `finalize_compaction_close` can reuse it without
/// pulling state internals into the dispatcher.
fn close_agent_tabs_in_cz(
    center_zone: &mut CenterZoneState,
    host_id: &str,
    agent_id: &AgentId,
) -> HashSet<TabId> {
    let remove_ids: Vec<_> = center_zone
        .all_tabs()
        .filter(|(_, tab)| {
            matches!(
                &tab.content,
                TabContent::Chat { agent_ref: Some(ar), .. }
                    if ar.host_id == host_id && ar.agent_id == *agent_id
            )
        })
        .map(|(_, tab)| tab.id)
        .collect();
    let mut removed = HashSet::new();
    for id in remove_ids {
        center_zone.close(id);
        if center_zone.tab(id).is_none() {
            removed.insert(id);
        }
    }
    removed
}

fn close_host_runtime_tabs_in_cz(
    center_zone: &mut CenterZoneState,
    host_id: &str,
) -> HashSet<TabId> {
    let remove_ids: Vec<_> = center_zone
        .all_tabs()
        .filter(|(_, tab)| match &tab.content {
            TabContent::Chat {
                agent_ref,
                pending_team_member,
            } => {
                agent_ref
                    .as_ref()
                    .is_some_and(|agent_ref| agent_ref.host_id == host_id)
                    || pending_team_member
                        .as_ref()
                        .is_some_and(|pending| pending.host_id == host_id)
            }
            TabContent::Diff {
                host_id: tab_host, ..
            }
            | TabContent::Comments {
                host_id: tab_host, ..
            } => tab_host == host_id,
            TabContent::Workflow { agent_ref, .. } => agent_ref.host_id == host_id,
            TabContent::Home | TabContent::AgentMonitor | TabContent::File { .. } => false,
        })
        .map(|(_, tab)| tab.id)
        .collect();
    let mut removed = HashSet::new();
    for id in remove_ids {
        center_zone.close(id);
        if center_zone.tab(id).is_none() {
            removed.insert(id);
        }
    }
    removed
}

#[derive(Clone, Debug, PartialEq)]
pub struct SessionInfo {
    pub host_id: String,
    pub summary: SessionSummary,
}

/// What a `BrowseDialog` is opening for. Lets the same browser component serve
/// different consumers (project create, future: add-root, pick-file, ...).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BrowsePurpose {
    OpenProject,
    AddRoot { project_id: ProjectId },
}

#[derive(Clone, Debug)]
pub struct BrowseDialogState {
    pub host_id: ArcRwSignal<String>,
    pub browse_stream: ArcRwSignal<StreamPath>,
    pub purpose: BrowsePurpose,
    pub include_hidden: ArcRwSignal<bool>,
    /// Set once `HostBrowseOpened` arrives.
    pub platform: ArcRwSignal<Option<HostPlatform>>,
    pub separator: ArcRwSignal<char>,
    pub home: ArcRwSignal<Option<HostAbsPath>>,
    pub current_path: ArcRwSignal<Option<HostAbsPath>>,
    pub parent: ArcRwSignal<Option<HostAbsPath>>,
    pub entries: ArcRwSignal<Vec<HostBrowseEntry>>,
    pub error: ArcRwSignal<Option<HostBrowseErrorPayload>>,
    pub loading: ArcRwSignal<bool>,
}

/// Snapshot of center-zone UI state for a single project. Persisted while the
/// user browses around so that flipping back to a project restores exactly the
/// view they left — and opening a different project does not leak state from
/// another.
#[derive(Clone, Debug, Default)]
pub struct ProjectViewMemory {
    pub center_zone: Option<CenterZoneState>,
    pub active_terminal: Option<ActiveTerminalRef>,
    pub open_files: HashMap<FileResourceKey, OpenFile>,
    pub diff_contents: HashMap<DiffKey, DiffViewState>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct ActiveProjectRef {
    pub host_id: String,
    pub project_id: ProjectId,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PendingAgentSessionSettings {
    id: u64,
    values: SessionSettingsValues,
}

type PendingAgentSessionSettingsByProject =
    HashMap<(String, Option<ProjectId>), VecDeque<PendingAgentSessionSettings>>;

/// Latest server-emitted Add-report shuffle suggestion plus a monotonic
/// `serial` so the open dialog can apply only fresh suggestions and
/// ignore stale ones still sitting in state on re-open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TeamMemberShuffleSuggestionEntry {
    pub suggestion: TeamMemberShuffleSuggestion,
    pub serial: u64,
}

/// Per-project filter state for the Agents panel. Stored per active project
/// (keyed by `Option<ActiveProjectRef>`, where `None` represents the Home
/// project) so user toggles persist across project switches for the life of
/// the app.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AgentsPanelFilters {
    pub hide_sub_agents: bool,
    pub hide_inactive: bool,
    pub show_other_projects: bool,
}

impl AgentsPanelFilters {
    /// Test-only helper encoding the `ContextualDefault` projection (Home shows
    /// all projects, in-project shows current only). Production code derives the
    /// effective filters from the server-owned sidebar preferences via
    /// `agents_panel::sidebar_to_panel_filters`.
    #[cfg(test)]
    pub fn defaults_for(project: Option<&ActiveProjectRef>) -> Self {
        Self {
            hide_sub_agents: false,
            hide_inactive: false,
            show_other_projects: project.is_none(),
        }
    }
}

/// Short-lived, non-persisted optimistic overlay for in-flight Agents-view
/// preference mutations. Each field is `Some` only while a change to that
/// preference domain has been sent to the server and the confirming
/// `AgentsViewPreferencesNotify` (or a fresh bootstrap snapshot) has not yet
/// arrived. The overlay is layered on top of the server snapshot so the UI
/// reacts instantly, but it is never written to disk and can never become a
/// durable second source of truth — which is precisely what kept the Agents
/// tab from flickering before this design.
///
/// Reconciliation is **drop-on-any-authoritative-snapshot**: an
/// `AgentsViewPreferencesNotify` (or a primary-host bootstrap) is a *full*
/// snapshot, so once one arrives the whole overlay is discarded — the server
/// value wins even when it differs from the optimistic one (the server
/// canonicalizes filter enum order and keeps historical session keys in manual
/// order, so an exact-equality check would never match and the overlay would
/// stick, masking later server changes). A safety timeout
/// (`OVERLAY_RECONCILE_TIMEOUT_MS`) drops a stale overlay if a send is dropped
/// and no snapshot ever arrives. See `dev-docs/26-agent-organization.md`
/// §4.3 / §7.4 / §12.1.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AgentsViewOverlay {
    pub filters: Option<AgentsViewFilters>,
    pub sort_mode: Option<AgentSortMode>,
    pub group_mode: Option<AgentGroupMode>,
    pub density: Option<AgentListDensity>,
    /// Deprecated protocol preference retained in the overlay shape for
    /// compatibility; current UI no longer sets or applies it.
    pub hide_finished: Option<bool>,
    pub manual_order: Option<Vec<AgentOrderKey>>,
    /// Optimistic override for the server-owned sidebar selectors (hide
    /// inactive / hide sub-agents / project visibility). `Some` only while a
    /// `SetSidebarPreferences` send is in flight; dropped wholesale on the next
    /// authoritative snapshot like every other domain.
    pub sidebar: Option<AgentsSidebarPreferences>,
    /// Optimistic override for the active Smart View id (dev-docs/26 §12.4):
    /// selecting a view sets the inner value to `Some(id)` so the switcher
    /// highlights instantly, while editing the query directly sets it to `None`
    /// so the highlight clears (the query no longer matches a named view). The
    /// outer `Option` follows the same domain-overlay convention as the other
    /// fields: `None` means "no override, read the server snapshot". Dropped
    /// wholesale on the next authoritative snapshot like every other domain.
    pub active_view_id: Option<Option<SmartViewId>>,
}

impl AgentsViewOverlay {
    fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// Per-project filter state for the Sessions/History panel. Stored per
/// active project (keyed by `Option<ActiveProjectRef>`, where `None`
/// represents the Home project) so user toggles persist across project
/// switches for the life of the app.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionsPanelFilters {
    pub show_child_sessions: bool,
    pub show_other_projects: bool,
}

impl SessionsPanelFilters {
    pub fn defaults_for(project: Option<&ActiveProjectRef>) -> Self {
        Self {
            show_child_sessions: false,
            show_other_projects: project.is_none(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ActiveAgentRef {
    pub host_id: String,
    pub agent_id: AgentId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActiveTerminalRef {
    pub host_id: String,
    pub terminal_id: TerminalId,
}

/// In-flight `WorkbenchCreate` request awaiting the matching `ProjectNotify::
/// Upsert`. The dispatcher correlates by `(host_id, parent_project_id, branch)`
/// — see §3.3 of `dev-docs/18-workbenches.md` — and on a match switches the
/// active project to the new workbench id, then removes the entry. A
/// `CommandError` for `WorkbenchCreate` marks the oldest non-failed entry for
/// the host with the error message (the error carries no parent/branch
/// correlation); the create modal consumes errored entries to surface the
/// failure inline. Entries are time-bounded by
/// [`PENDING_WORKBENCH_CREATE_TTL_MS`] so a mis-correlated or orphaned entry
/// cannot linger and trigger a spurious active-project switch much later.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingWorkbenchCreate {
    pub host_id: String,
    pub parent_project_id: ProjectId,
    pub branch: GitBranchName,
    /// Wall-clock ms (`Date.now()`) when the request was sent. See
    /// [`PendingWorkbenchCreate::is_stale`].
    pub requested_at_ms: u64,
    /// Error message from a `CommandError` for `WorkbenchCreate` on this
    /// host. `None` while the create is still in flight.
    pub error: Option<String>,
}

/// How long an in-flight workbench create stays correlatable. Past this the
/// entry is purged on the next touch of `pending_workbench_creates`.
pub const PENDING_WORKBENCH_CREATE_TTL_MS: u64 = 5 * 60 * 1000;

impl PendingWorkbenchCreate {
    pub fn is_stale(&self, now_ms: u64) -> bool {
        now_ms.saturating_sub(self.requested_at_ms) > PENDING_WORKBENCH_CREATE_TTL_MS
    }
}

/// Current wall-clock in ms. Zero on non-wasm builds (native logic tests
/// never exercise the staleness path).
pub fn now_ms() -> u64 {
    #[cfg(target_arch = "wasm32")]
    {
        js_sys::Date::now() as u64
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        0
    }
}

/// A pending request to run a workflow that declares inputs. The Workflows
/// panel Run button and the command palette both populate this; a global modal
/// renders one field per declared input and triggers the run on submit. A
/// workflow with no declared inputs never produces one of these — it runs in a
/// single click without a modal.
#[derive(Clone, Debug, PartialEq)]
pub struct WorkflowRunRequest {
    pub host_id: String,
    pub workflow_id: WorkflowId,
    pub project_id: Option<ProjectId>,
    pub name: String,
    pub inputs: Vec<WorkflowInputSpec>,
}

/// A workflow command failure surfaced inline in the Workflows panel. Keyed by
/// host. `request_kind` is the originating frame (`WorkflowRefresh`,
/// `TriggerWorkflow`, or `CancelWorkflow`) so the panel clears it on the next
/// successful notify for that operation.
#[derive(Clone, Debug, PartialEq)]
pub struct WorkflowPanelError {
    pub request_kind: FrameKind,
    pub message: String,
}

#[derive(Clone)]
pub struct AppState {
    pub configured_hosts: RwSignal<Vec<ConfiguredHost>>,
    pub selected_host_id: RwSignal<Option<String>>,
    pub host_streams: RwSignal<HashMap<String, StreamPath>>,
    pub connection_statuses: RwSignal<HashMap<String, ConnectionStatus>>,
    pub host_lifecycle_statuses: RwSignal<HashMap<String, RemoteHostLifecycleStatus>>,
    pub command_errors_by_host: RwSignal<HashMap<String, String>>,
    pub projects: RwSignal<Vec<ProjectInfo>>,
    pub agents: RwSignal<Vec<AgentInfo>>,
    pub sessions: RwSignal<Vec<SessionInfo>>,
    pub active_project: RwSignal<Option<ActiveProjectRef>>,
    /// Cold-start selection restored only after its owning host has published
    /// the project catalog. Catalog ownership and UI navigation are distinct;
    /// bootstrap must not guess the latter from whichever agent arrives first.
    pub pending_active_project_restore: RwSignal<Option<ActiveProjectRef>>,
    /// Derived from `center_zone.composer_owner()`. The focused pane's active
    /// chat wins; when a file pane is focused, the other pane's active chat
    /// remains the singleton composer owner. Read-only by design.
    pub active_agent: Memo<Option<ActiveAgentRef>>,
    pub chat_rows: RwSignal<HashMap<AgentId, Vec<ChatRowHandle>>>,
    pub chat_tool_rows: RwSignal<HashMap<AgentId, HashMap<ToolCallId, ChatRowId>>>,
    /// Per-agent index from server-issued `ChatMessageId` to the row that
    /// carries it. Populated by `push_chat_entry` when the entry's
    /// `message.message_id` is present, and consulted when a
    /// `ChatEvent::MessageMetadataUpdated` arrives so the existing row's
    /// token/model/context fields can be patched in place instead of
    /// appending a duplicate row. Cleared anywhere `chat_rows` is cleared
    /// (host runtime reset, agent close, agent bootstrap snapshot).
    pub chat_message_rows: RwSignal<HashMap<AgentId, HashMap<ChatMessageId, ChatRowId>>>,
    /// Server-owned prior-history availability for each agent. The server
    /// sends only this indicator in `AgentBootstrap`; actual prior transcript
    /// rows are fetched explicitly with `FetchSessionHistory` and prepended
    /// when `SessionHistory` arrives.
    pub session_history: RwSignal<HashMap<AgentId, SessionHistoryState>>,
    pub streaming_text: RwSignal<HashMap<AgentId, StreamingState>>,
    /// Latest `ToolProgress` snapshot per tool call, keyed by the owning
    /// agent and tool call id. The single source of truth for live tool
    /// activity (workflow runs, sub-agents): tool cards and the workflow
    /// tab look snapshots up reactively here — progress is deliberately
    /// NOT stored on `ToolRequestEntry`, which keyed `<For>` rows would
    /// render as a frozen snapshot. The inner signal lets an open card
    /// update without re-rendering the whole map. Cleared anywhere
    /// `chat_tool_rows` is cleared.
    pub tool_progress: RwSignal<HashMap<(AgentId, ToolCallId), ArcRwSignal<ToolProgressData>>>,
    pub chat_input: RwSignal<String>,
    pub task_lists: RwSignal<HashMap<AgentId, TaskList>>,
    /// Per-agent Tycode orchestration event log (sub-agent/workflow progress),
    /// chronological. Appended to as `ChatEvent::Orchestration` events arrive
    /// and as history replays; the orchestration panel folds it into a compact
    /// progress tree. These are Tycode-internal orchestration nodes, not
    /// first-class Tyde agents. Cleared with the agent's other per-agent state.
    pub orchestration: RwSignal<HashMap<AgentId, Vec<OrchestrationRecord>>>,
    /// Server-owned per-agent activity stats (running tool-call count, token
    /// usage, last output line), keyed by the owning `(host_id, agent_id)` so two
    /// hosts that hand out the same agent-id string can't collide. Populated from
    /// `AgentActivityStats` frames and agent bootstrap; the frontend renders
    /// these verbatim and never derives tool/token counts from chat rows. Cleared
    /// on agent close and host disconnect.
    pub agent_activity_stats: RwSignal<HashMap<ActiveAgentRef, AgentActivityStats>>,
    /// Server-authoritative task token rollups (root agent + descendants),
    /// keyed by the owning `(host_id, root_agent_id)`. Populated from
    /// `TaskTokenUsage` host-stream frames and host bootstrap; the frontend
    /// renders totals and breakdown rows verbatim and never sums entries
    /// itself. Cleared on agent close and host disconnect; a host bootstrap
    /// replaces the host's full set.
    pub task_token_usage: RwSignal<HashMap<ActiveAgentRef, TaskTokenUsagePayload>>,
    pub center_zone: RwSignal<CenterZoneState>,
    /// Window-local split ratio preference. The active project's split also
    /// carries its own ratio inside CenterLayout; only this scalar is suitable
    /// for cold local-storage persistence.
    pub center_split_ratio: RwSignal<SplitRatio>,
    /// Tabs whose content components are currently mounted, MRU-first. The
    /// active tab is always at the front; the next slot (if any) is the most
    /// recently active tab before it. Tabs absent from this list have their
    /// content unmounted entirely — no DOM, no reactive subscriptions. This
    /// keeps "many tabs open" cheap: we pay for at most `TAB_LRU_CAPACITY`
    /// component trees regardless of how many tabs the user has opened.
    /// Driven by an Effect in `App`; `mounted_tab_ids` additionally pins every
    /// pane's active tab so one pane cannot evict the other.
    pub tab_lru: RwSignal<Vec<TabId>>,
    pub tab_scroll_state: RwSignal<HashMap<TabId, TabScrollState>>,
    pub tabs_enabled: RwSignal<bool>,
    pub left_dock: RwSignal<DockVisibility>,
    pub right_dock: RwSignal<DockVisibility>,
    pub right_tab: RwSignal<RightTab>,
    pub bottom_dock: RwSignal<DockVisibility>,
    pub file_tree: RwSignal<HashMap<ProjectId, Vec<ProjectRootListing>>>,
    pub git_status: RwSignal<HashMap<ProjectId, Vec<ProjectRootGitStatus>>>,
    /// Server-authored code-intelligence overview per project (full-replacement
    /// frame `code_intel_overview`), keyed by the owning `(host_id, project_id)`
    /// so two hosts sharing a project-id string can't collide. Drives the
    /// Files-explorer status footer. The frontend renders this verbatim; it never
    /// infers provider state from open files or extensions.
    pub code_intel_overview: RwSignal<HashMap<ActiveProjectRef, CodeIntelOverviewPayload>>,
    pub open_files: RwSignal<HashMap<FileResourceKey, OpenFile>>,
    /// Invocation-time routing for cold file opens and in-place refreshes.
    /// Open wins over RefreshInPlace so a failed refresh marker cannot swallow
    /// the user's next explicit open.
    pub pending_file_opens: RwSignal<HashMap<FileResourceKey, PendingFileOpen>>,
    /// Server-pushed code-intelligence state, keyed by `(host_id, project_id,
    /// path)`. Kept separate from `Token`/syntax data on purpose (spec §6): the
    /// per-row token path has a wasm test guarding against text mangling, and
    /// semantic decorations must never ride that path.
    pub code_intel: RwSignal<HashMap<CodeIntelKey, CodeIntelFileState>>,
    pub diff_contents: RwSignal<HashMap<DiffKey, DiffViewState>>,
    pub terminals: RwSignal<Vec<TerminalInfo>>,
    pub active_terminal: RwSignal<Option<ActiveTerminalRef>>,
    pub transient_events: RwSignal<HashMap<AgentId, Vec<TransientEvent>>>,
    pub browse_dialog: RwSignal<Option<BrowseDialogState>>,
    /// Per-project snapshots of center-zone state. Updated whenever the user
    /// switches away from a project; consulted on switch-in to restore.
    pub project_view_memory: RwSignal<HashMap<ActiveProjectRef, ProjectViewMemory>>,
    pub command_palette_open: RwSignal<bool>,
    pub settings_open: RwSignal<bool>,
    /// When set, the settings panel jumps to the tab with this label (e.g.
    /// "Backends") the next time it renders. Used to deep-link from onboarding
    /// CTAs. Cleared by the panel once consumed.
    pub settings_tab_request: RwSignal<Option<&'static str>>,
    /// Current step of the guided help tour overlay, `None` when the tour is
    /// closed. The Help button on the home screen starts it at step 0.
    pub help_tour_step: RwSignal<Option<usize>>,
    pub feedback_open: RwSignal<bool>,
    pub find_bar_open: RwSignal<bool>,
    /// Which left-dock tab is active (Files / Git / Search).
    pub left_tab: RwSignal<LeftTab>,
    /// Persistent state for the project-wide Search panel.
    pub search_state: RwSignal<ProjectSearchUiState>,
    /// Persistent state for the find-references results panel (M5).
    pub references_state: RwSignal<ProjectReferencesUiState>,
    /// Bumped to request the Search panel focus (and select) its query input —
    /// e.g. on the Cmd/Ctrl+Shift+F shortcut or the "search in folder" action.
    pub search_focus_seq: RwSignal<u32>,
    /// When set, exactly one file occurrence should scroll so the given
    /// 1-based line is visible. Consumed by the matching TabId.
    pub pending_goto_line: RwSignal<Option<(TabId, u32)>>,
    /// Like `pending_goto_line` but addressed by an absolute file **byte
    /// offset** (from a go-to-definition target, whose range is byte-based). The
    /// file view converts it to a line via its `FileLines` and consumes it. Kept
    /// separate so the existing line-based goto machinery and its tests are
    /// untouched.
    pub pending_goto_offset: RwSignal<Option<(TabId, u32)>>,
    /// Monotonic source of `navigate_id` / `hover_id` domain ids for on-demand
    /// code-intel requests (cf. `search_id`). Bumped per request.
    pub code_intel_request_seq: RwSignal<u64>,
    /// Context for the most recent `code_intel_navigate` the client sent. A
    /// result is acted on only when it still matches this context (id + owning
    /// host/project + source file open at the same rendered version).
    pub code_intel_navigate_ctx: RwSignal<Option<CodeIntelNavigateContext>>,
    /// The most recent `hover_id` the client sent. Supersedes older hovers.
    pub code_intel_active_hover: RwSignal<u64>,
    /// The current hover popover, or `None` when nothing is hovered. The
    /// `HoverPopover` component renders from this signal (no `window.*`).
    pub code_intel_hover: RwSignal<Option<HoverPopover>>,
    /// Transient per-tab code-intel notice (see [`CodeIntelNotice`]); at most
    /// one at a time — a newer notice replaces the last.
    pub code_intel_notice: RwSignal<Option<CodeIntelNotice>>,
    /// True while the go-to-definition modifier is held. Mirrors the existing
    /// Cmd/Ctrl-click convention and is cleared on blur/visibility changes.
    pub cmd_held: RwSignal<bool>,
    /// The file (and rendered version) the user most recently interacted with in
    /// a file view, so the F12 keybinding (which has no file context of its own)
    /// can navigate from the current caret in that file.
    pub code_intel_focus: RwSignal<Option<FileFocus>>,
    pub host_settings_by_host: RwSignal<HashMap<String, HostSettings>>,
    pub backend_setup_by_host: RwSignal<HashMap<String, Vec<BackendSetupInfo>>>,
    pub agent_message_queue: RwSignal<HashMap<AgentId, Vec<QueuedMessageEntry>>>,
    pub agent_turn_active: RwSignal<HashMap<AgentId, bool>>,
    pub draft_backend_override: RwSignal<Option<BackendKind>>,
    pub draft_custom_agent_id: RwSignal<Option<CustomAgentId>>,
    /// Server-owned launch profile catalog keyed by host id. Seeded by
    /// `HostBootstrap` and replaced wholesale by `LaunchProfileCatalogNotify`.
    /// The new-chat menus render these entries directly instead of deriving
    /// launch options from raw backend lists.
    pub launch_profile_catalog: RwSignal<HashMap<String, LaunchProfileCatalog>>,
    /// Launch profile selected for the pending new chat. Set from a ready
    /// catalog profile (never parsed from the id) and sent with
    /// `SpawnAgentParams::New`.
    pub draft_launch_profile_id: RwSignal<Option<LaunchProfileId>>,
    pub session_schemas: RwSignal<HashMap<String, HashMap<BackendKind, SessionSchemaEntry>>>,
    pub schemas_loaded_for_host: RwSignal<HashMap<String, bool>>,
    /// Host-level deep-config schemas, keyed by host id then backend kind.
    /// Backends without deep config are absent. Values live in
    /// `host_settings_by_host` (`HostSettings.backend_config`).
    pub backend_config_schemas:
        RwSignal<HashMap<String, HashMap<BackendKind, protocol::BackendConfigSchema>>>,
    /// Server-owned snapshots of each backend's *current native* configuration,
    /// keyed by host id then backend kind. These are the backend's own source of
    /// truth (read by the server), distinct from the Tyde-managed overrides in
    /// `HostSettings.backend_config`. Backends without deep config are absent.
    pub backend_config_snapshots:
        RwSignal<HashMap<String, HashMap<BackendKind, protocol::BackendConfigSnapshot>>>,
    /// Server-owned subscription-capacity snapshots, keyed by host id then
    /// backend kind. Replayed on host-stream subscribe and re-emitted on every
    /// change, so initial state and live updates travel the same path. Capacity
    /// is account-scoped but arrives on a per-agent pipe, so the server keys it
    /// by (host, backend) and the frontend must never key it by agent. The
    /// frontend renders these verbatim: it runs no freshness clock (staleness is
    /// the server's `CapacityFreshness` verdict), keeps no cache, and never
    /// infers capacity from Tyde's own token usage. Cleared on host disconnect.
    pub backend_capacity:
        RwSignal<HashMap<String, HashMap<BackendKind, protocol::BackendCapacitySnapshot>>>,
    /// Server-owned backend-native settings snapshots (JSON-schema-driven,
    /// grouped), keyed by host id then backend kind. Each snapshot carries the
    /// backend's current settings document and grouped schemas, or an explicit
    /// unavailable status with a reason. Distinct from `backend_config_snapshots`
    /// (typed flat fields) and from the Tyde-managed overrides in
    /// `HostSettings.backend_config`. Backends without native settings are absent.
    pub backend_native_settings:
        RwSignal<HashMap<String, HashMap<BackendKind, protocol::BackendNativeSettingsSnapshot>>>,
    /// In-flight/failed state for backend-native settings saves, keyed by host id
    /// then backend kind. A native save sends the whole settings document, so a
    /// second edit based on the same (now stale) snapshot would clobber the first.
    /// While a save is `Pending`, the native controls are disabled until the
    /// server publishes a newer snapshot (detected by the settings document
    /// differing from the `base` the save was applied to) — so the "saving" state
    /// stays a projection of server-owned state, not an invented client model.
    pub native_settings_save_state:
        RwSignal<HashMap<String, HashMap<BackendKind, NativeSettingsSaveState>>>,
    /// Host id for which the next `NewTerminal` should steal focus. Set when the
    /// user clicks Install/Sign-in; consumed in the dispatcher so the new
    /// terminal becomes active even if another terminal was already selected.
    pub pending_terminal_focus: RwSignal<Option<String>>,
    pub agent_session_settings: RwSignal<HashMap<AgentId, SessionSettingsValues>>,
    /// User-visible settings submitted for a draft whose `NewAgent` echo has
    /// not arrived yet. The host stream publishes agent identity before the
    /// agent stream publishes authoritative effective settings; retaining the
    /// submitted values prevents that expected gap from masquerading as Auto.
    pub pending_agent_session_settings: RwSignal<PendingAgentSessionSettingsByProject>,
    next_pending_agent_session_settings_id: RwSignal<u64>,
    pub draft_session_settings: RwSignal<SessionSettingsValues>,
    /// Whether the user has actually edited `draft_session_settings` in the
    /// session-settings bar. Reset when a new-chat draft starts. When a launch
    /// profile is selected and this is `false`, `spawn_new_chat` omits explicit
    /// `session_settings` so the server-owned profile resolution is authoritative
    /// (a stale copy of the profile's settings must not override a changed
    /// server profile).
    pub draft_session_settings_dirty: RwSignal<bool>,
    pub font_size: RwSignal<u32>,
    pub theme: RwSignal<String>,
    pub font_family: RwSignal<String>,
    /// Active syntect theme name (e.g. "base16-ocean.dark"). Drives both the
    /// file viewer and diff viewer's syntax coloring. Persists across sessions.
    pub syntax_theme: RwSignal<String>,
    pub diff_view_mode: RwSignal<DiffViewMode>,
    pub diff_context_mode: RwSignal<DiffContextMode>,
    pub tool_output_mode: RwSignal<ToolOutputMode>,
    pub custom_agents: RwSignal<HashMap<String, HashMap<CustomAgentId, CustomAgent>>>,
    pub mcp_servers: RwSignal<HashMap<String, HashMap<McpServerId, McpServerConfig>>>,
    pub steering: RwSignal<HashMap<String, HashMap<SteeringId, Steering>>>,
    pub skills: RwSignal<HashMap<String, HashMap<SkillId, Skill>>>,
    pub workflow_summaries: RwSignal<HashMap<String, Vec<WorkflowSummary>>>,
    pub workflow_diagnostics: RwSignal<HashMap<String, Vec<WorkflowDiagnostic>>>,
    pub workflow_runs: RwSignal<HashMap<String, HashMap<WorkflowRunId, WorkflowRunSnapshot>>>,
    /// Server-sent workflow catalog directories (global + per project root),
    /// keyed by host_id. Seeded by `HostBootstrap` and replaced wholesale by
    /// `WorkflowNotify`. The empty-state teaching copy and the authoring CTA
    /// read the real paths from here instead of reconstructing `.tyde/workflows`
    /// by string convention.
    pub workflow_locations: RwSignal<HashMap<String, Vec<WorkflowCatalogLocation>>>,
    /// Pending run-with-inputs request driving the global workflow inputs modal.
    /// `Some` while the modal is open; cleared on submit or cancel.
    pub workflow_run_request: RwSignal<Option<WorkflowRunRequest>>,
    /// Inline workflow command failures, keyed by host_id. Written by the
    /// `CommandError` dispatch path for workflow request kinds and cleared on the
    /// next successful workflow notify for the failed operation.
    pub workflow_command_errors: RwSignal<HashMap<String, WorkflowPanelError>>,
    /// Host-scoped team records, keyed by host_id then TeamId. Populated from
    /// `TeamNotify::Upsert` and pruned by `TeamNotify::Delete`.
    pub teams: RwSignal<HashMap<String, HashMap<TeamId, Team>>>,
    /// Host-scoped team member records. Members are looked up by id when
    /// rendering rosters and detail views; teams are joined via member.team_id.
    pub team_members: RwSignal<HashMap<String, HashMap<TeamMemberId, TeamMember>>>,
    /// Runtime team-member bindings: `current_agent_id`, status, last-active.
    /// Server emits these as `TeamMemberBindingNotify`. After a restart every
    /// binding starts with `current_agent_id: None` until the member is
    /// reactivated.
    pub team_member_bindings:
        RwSignal<HashMap<String, HashMap<TeamMemberId, TeamMemberBindingPayload>>>,
    /// Server-owned team creation catalog records. The frontend renders these
    /// options but does not define preset/template semantics locally.
    pub team_preset_catalogs: RwSignal<HashMap<String, TeamPresetCatalog>>,
    /// Server-owned in-progress team drafts, keyed by host then draft id.
    pub team_drafts: RwSignal<HashMap<String, HashMap<TeamDraftId, TeamDraft>>>,
    /// Latest server-emitted Add-report shuffle suggestion per host/team.
    /// The frontend bumps `serial` each time a notify arrives so the open
    /// dialog can detect a fresh suggestion and apply it without
    /// re-applying stale ones on re-open. Suggestions are ephemeral
    /// (never replayed on host attach).
    pub team_member_shuffle_suggestions:
        RwSignal<HashMap<String, HashMap<TeamId, TeamMemberShuffleSuggestionEntry>>>,
    /// Durable Agents-tab view preferences (filters, sort, group, density,
    /// manual order, plus deprecated protocol fields). The server is the single
    /// source of truth:
    /// the primary local host emits a `Some` snapshot in its bootstrap and via
    /// `AgentsViewPreferencesNotify`. This signal is *not* pruned on host
    /// cleanup, so a remount/reconnect re-reads the same server-fed base rather
    /// than re-deriving a fresh local map — the root fix for the Agents-tab
    /// flicker. See `dev-docs/26-agent-organization.md` §5.2 / §8.
    pub agents_view_preferences: RwSignal<AgentsViewPreferencesSnapshot>,
    /// Configured-host id of the primary local host that owns
    /// `agents_view_preferences`. Set when a bootstrap/notify carries a `Some`
    /// snapshot; preference mutations are routed back to this host's stream.
    pub agents_view_preferences_host: RwSignal<Option<String>>,
    /// Non-persisted optimistic overlay for in-flight preference mutations.
    pub pending_agents_view_overlay: RwSignal<AgentsViewOverlay>,
    /// Monotonic generation bumped on every overlay mutation. The safety
    /// timeout captures the generation it armed for and only drops the overlay
    /// if no newer mutation has since superseded it.
    pub agents_view_overlay_generation: RwSignal<u64>,
    pub sessions_panel_filters: RwSignal<HashMap<Option<ActiveProjectRef>, SessionsPanelFilters>>,
    /// Per-review full state. Server is the source of truth: a `ReviewView`
    /// subscribes to `/review/<id>` and dispatch applies `ReviewEvent`
    /// deltas to the entry. The first event on subscribe is always
    /// `ReviewEvent::Snapshot` which seeds (or replaces) the entry.
    pub reviews: RwSignal<HashMap<ReviewId, Review>>,
    /// Per-project review summary lists, populated from
    /// `ProjectEventPayload::ReviewListChanged` on each project stream.
    /// Used by the project rail / git panel indicator to show "open
    /// review against this working tree" without subscribing to every
    /// `/review/<id>` stream.
    pub review_summaries: RwSignal<HashMap<ProjectId, Vec<ReviewSummary>>>,
    /// True while a `ReviewCreate` for the given (host, project) is in
    /// flight and the server hasn't yet echoed a `ReviewListChanged` that
    /// includes a fresh review. Disables the "Review changes" button on
    /// the agent header so the user can't fire a second creation while
    /// the first is mid-flight. Cleared by the dispatch handler when a
    /// summary list refresh arrives that wasn't already known. No
    /// optimistic UI: we never synthesize a Review record on the
    /// frontend.
    pub review_create_pending: RwSignal<HashMap<(String, ProjectId), u32>>,
    /// Per-review action gate: true while a `ReviewAction` is in flight
    /// for that review id, used to disable buttons until the server
    /// echoes back the corresponding event. Each entry is a small bitmap
    /// of the actions awaiting echo so independent buttons (Submit,
    /// Cancel, Run AI, …) gate independently.
    pub review_action_pending: RwSignal<HashMap<ReviewId, ReviewActionGate>>,
    /// Per-(review, target) gate for actions that operate on a specific
    /// comment, suggestion, or composer instance. Held in a `HashSet` so
    /// each in-flight action keys to its own row, allowing independent
    /// rows to gate independently. Entries are cleared by dispatch when
    /// the matching `ReviewEvent` echoes back, or on
    /// `ReviewEvent::Error` whose context matches.
    pub review_action_target_pending: RwSignal<HashSet<(ReviewId, ReviewActionTarget)>>,
    /// Agents whose compaction request is in flight, keyed by the old
    /// agent id, with a snapshot of identifying fields captured at
    /// compaction-start time. The fingerprint lets the `NewAgent`
    /// dispatcher tell which incoming user-origin agent is the
    /// replacement (and so should NOT auto-open a competing tab) versus
    /// an unrelated spawn. The Agents panel renders these agents with a
    /// running-blue "Compacting…" pill and hides the Compact button so
    /// the user can't double-fire. Cleared by
    /// `finish_compaction_success` / `finish_compaction_failure`.
    pub compaction_in_progress: RwSignal<HashMap<AgentId, CompactionOldInfo>>,
    /// Last non-fatal compaction error per agent, keyed by the agent the
    /// user asked to compact. Rendered as an inline message on the agent
    /// card; cleared on the next successful start.
    pub compaction_errors: RwSignal<HashMap<AgentId, String>>,
    /// `Completed` notify can arrive before the replacement's `NewAgent`
    /// echo is dispatched. When that happens we stash `new → old` here
    /// keyed by `(host_id, new_agent_id)`, and the `NewAgent` arm flushes
    /// the entry by calling `finish_compaction_success`.
    pub compaction_pending_completion: RwSignal<HashMap<(String, AgentId), AgentId>>,
    /// Defensive belt for ordering inversions. Under the current
    /// server contract the event order is `NewAgent (replacement) →
    /// Completed (on old, still-valid stream) → AgentClosed (old)`,
    /// so by the time `AgentClosed` lands `compaction_in_progress`
    /// has already been cleared by `Completed` and the deferred-close
    /// set stays empty. We keep the set so that if the server ever
    /// inverts ordering for any reason — `AgentClosed` before
    /// `Completed` — we still preserve the user's chat tab until
    /// `Completed` retargets it. Drained at
    /// `finish_compaction_success` time.
    pub compaction_pending_close: RwSignal<HashSet<(String, AgentId)>>,
    /// Latest server-pushed `MobileAccessState` snapshot per host. The
    /// payload carries broker status, the pairing-lifecycle phase
    /// (`Idle | Active | Consumed | Expired | Cancelled | Failed`), and
    /// the paired-device list. The Mobile settings tab reads from this
    /// to render pairing status / device list. Server is the source of
    /// truth; the frontend never synthesises entries.
    pub mobile_access_state: RwSignal<HashMap<String, MobileAccessStatePayload>>,
    /// Latest server-pushed `MobilePairingOffer` per host. Contains the
    /// `qr_uri` we render as a QR code. Cleared when the pairing
    /// lifecycle transitions out of Active (Consumed / Expired /
    /// Cancelled / Failed) so a stale QR isn't left lying around.
    pub mobile_pairing_offer: RwSignal<HashMap<String, MobilePairingOfferPayload>>,
    /// Per-host bit: true while a `MobilePairingStart` is in flight and
    /// we haven't yet seen the server-confirmed offer back. Used to
    /// disable the Start button so the user can't double-fire while
    /// the server is preparing the offer.
    pub mobile_pairing_start_pending: RwSignal<HashSet<String>>,
    /// In-flight `WorkbenchCreate` requests. The dispatcher uses these to
    /// correlate the resulting `ProjectNotify::Upsert` and switch the active
    /// project to the freshly-created workbench. See `PendingWorkbenchCreate`.
    pub pending_workbench_creates: RwSignal<Vec<PendingWorkbenchCreate>>,
    /// Managed remote hosts for which the Phase 2 safety net has already fired
    /// its one forced upgrade-and-reconnect after a `Reject{IncompatibleProtocol}`.
    /// This is ephemeral, frontend-owned *connect-control* state — a one-shot
    /// guard scoped to the current connection lifecycle — NOT mirrored
    /// server/business state. It guarantees "upgrade once, no loop": cleared on a
    /// successful `Welcome` (so a later legitimate reconnect can retry once) and
    /// intended to be cleared on an explicit user disconnect via
    /// [`AppState::clear_upgrade_attempted`]. It is deliberately NOT cleared on a
    /// transport-drop disconnect, since that would let a server that keeps
    /// rejecting re-trigger the upgrade indefinitely.
    pub upgrade_attempted: RwSignal<HashSet<String>>,
}

/// Snapshot of identifying fields captured for an agent at the moment
/// its compaction was kicked off. Used by `dispatch::apply_new_agent` to
/// recognize the server-spawned replacement (which shares these fields)
/// without needing a protocol-level lineage flag on `NewAgentPayload`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompactionOldInfo {
    pub host_id: String,
    pub project_id: Option<ProjectId>,
    pub custom_agent_id: Option<CustomAgentId>,
    pub backend_kind: BackendKind,
    /// Team-member id is read from `team_member_bindings` at start
    /// time: if the old agent is the live binding for a team member,
    /// the replacement's `NewAgent` payload will carry the same
    /// member id, giving a deterministic match.
    pub team_member_id: Option<TeamMemberId>,
}

/// Identifier for a per-row review action awaiting server echo. Used as
/// part of a `(ReviewId, ReviewActionTarget)` key so independent rows
/// (different comments, suggestions) gate independently of each other.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ReviewActionTarget {
    /// New comment via the inline composer for this review.
    AddComment,
    /// Update an existing comment.
    UpdateComment(ReviewCommentId),
    /// Delete an existing comment.
    DeleteComment(ReviewCommentId),
    /// Accept (or Edit & Accept) a pending AI suggestion.
    AcceptSuggestion(ReviewSuggestionId),
    /// Reject a pending AI suggestion.
    RejectSuggestion(ReviewSuggestionId),
}

/// Bitmask of review actions awaiting server echo. `0` means "nothing in
/// flight" — when the value drops back to `0` the entry can be removed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReviewActionGate {
    pub submit: bool,
    pub cancel: bool,
    pub start_ai: bool,
    pub add_comment: bool,
    /// True while a `ClearComments` action is in flight, awaiting the
    /// server's `Cleared` echo. Gates the inline "Clear" control.
    pub clear: bool,
}

impl ReviewActionGate {
    pub fn is_idle(&self) -> bool {
        !(self.submit || self.cancel || self.start_ai || self.add_comment || self.clear)
    }
}

impl AppState {
    pub fn new() -> Self {
        let initial_center_zone = CenterZoneState::default();
        // Pre-seed the LRU with the initial active tab so the first
        // CenterZone render mounts it immediately. Without this seed the
        // first frame paints with no mounted tab content, then the
        // tab-LRU Effect in `App` fires and adds the active tab — visible
        // as a one-frame flash of empty center zone on app boot.
        let initial_lru: Vec<TabId> = initial_center_zone
            .panes()
            .filter_map(|(_, pane)| pane.active_tab_id)
            .collect();
        let center_zone: RwSignal<CenterZoneState> = RwSignal::new(initial_center_zone);
        let active_agent: Memo<Option<ActiveAgentRef>> = Memo::new(move |_| {
            center_zone.with(|cz| {
                cz.composer_owner()
                    .and_then(|(_, tab_id)| cz.tab(tab_id))
                    .and_then(|tab| match &tab.content {
                        TabContent::Chat { agent_ref, .. } => agent_ref.clone(),
                        _ => None,
                    })
            })
        });

        Self {
            configured_hosts: RwSignal::new(Vec::new()),
            selected_host_id: RwSignal::new(None),
            host_streams: RwSignal::new(HashMap::new()),
            connection_statuses: RwSignal::new(HashMap::new()),
            host_lifecycle_statuses: RwSignal::new(HashMap::new()),
            command_errors_by_host: RwSignal::new(HashMap::new()),
            projects: RwSignal::new(Vec::new()),
            agents: RwSignal::new(Vec::new()),
            sessions: RwSignal::new(Vec::new()),
            active_project: RwSignal::new(None),
            pending_active_project_restore: RwSignal::new(load_active_project()),
            active_agent,
            chat_rows: RwSignal::new(HashMap::new()),
            chat_tool_rows: RwSignal::new(HashMap::new()),
            chat_message_rows: RwSignal::new(HashMap::new()),
            session_history: RwSignal::new(HashMap::new()),
            streaming_text: RwSignal::new(HashMap::new()),
            agent_activity_stats: RwSignal::new(HashMap::new()),
            task_token_usage: RwSignal::new(HashMap::new()),
            tool_progress: RwSignal::new(HashMap::new()),
            chat_input: RwSignal::new(String::new()),
            task_lists: RwSignal::new(HashMap::new()),
            orchestration: RwSignal::new(HashMap::new()),
            center_zone,
            center_split_ratio: RwSignal::new(SplitRatio::default()),
            tab_lru: RwSignal::new(initial_lru),
            tab_scroll_state: RwSignal::new(HashMap::new()),
            tabs_enabled: RwSignal::new(true),
            left_dock: RwSignal::new(DockVisibility::Visible),
            right_dock: RwSignal::new(DockVisibility::Visible),
            right_tab: RwSignal::new(RightTab::Agents),
            bottom_dock: RwSignal::new(DockVisibility::Hidden),
            file_tree: RwSignal::new(HashMap::new()),
            git_status: RwSignal::new(HashMap::new()),
            code_intel_overview: RwSignal::new(HashMap::new()),
            open_files: RwSignal::new(HashMap::new()),
            pending_file_opens: RwSignal::new(HashMap::new()),
            code_intel: RwSignal::new(HashMap::new()),
            diff_contents: RwSignal::new(HashMap::new()),
            terminals: RwSignal::new(Vec::new()),
            active_terminal: RwSignal::new(None),
            transient_events: RwSignal::new(HashMap::new()),
            browse_dialog: RwSignal::new(None),
            project_view_memory: RwSignal::new(HashMap::new()),
            command_palette_open: RwSignal::new(false),
            settings_open: RwSignal::new(false),
            settings_tab_request: RwSignal::new(None),
            help_tour_step: RwSignal::new(None),
            feedback_open: RwSignal::new(false),
            find_bar_open: RwSignal::new(false),
            left_tab: RwSignal::new(LeftTab::Files),
            search_state: RwSignal::new(ProjectSearchUiState::default()),
            references_state: RwSignal::new(ProjectReferencesUiState::default()),
            search_focus_seq: RwSignal::new(0),
            pending_goto_line: RwSignal::new(None),
            pending_goto_offset: RwSignal::new(None),
            code_intel_request_seq: RwSignal::new(0),
            code_intel_navigate_ctx: RwSignal::new(None),
            code_intel_active_hover: RwSignal::new(0),
            code_intel_hover: RwSignal::new(None),
            code_intel_notice: RwSignal::new(None),
            cmd_held: RwSignal::new(false),
            code_intel_focus: RwSignal::new(None),
            host_settings_by_host: RwSignal::new(HashMap::new()),
            backend_setup_by_host: RwSignal::new(HashMap::new()),
            agent_message_queue: RwSignal::new(HashMap::new()),
            agent_turn_active: RwSignal::new(HashMap::new()),
            draft_backend_override: RwSignal::new(None),
            draft_custom_agent_id: RwSignal::new(None),
            launch_profile_catalog: RwSignal::new(HashMap::new()),
            draft_launch_profile_id: RwSignal::new(None),
            session_schemas: RwSignal::new(HashMap::new()),
            schemas_loaded_for_host: RwSignal::new(HashMap::new()),
            backend_config_schemas: RwSignal::new(HashMap::new()),
            backend_config_snapshots: RwSignal::new(HashMap::new()),
            backend_capacity: RwSignal::new(HashMap::new()),
            backend_native_settings: RwSignal::new(HashMap::new()),
            native_settings_save_state: RwSignal::new(HashMap::new()),
            pending_terminal_focus: RwSignal::new(None),
            agent_session_settings: RwSignal::new(HashMap::new()),
            pending_agent_session_settings: RwSignal::new(HashMap::new()),
            next_pending_agent_session_settings_id: RwSignal::new(0),
            draft_session_settings: RwSignal::new(SessionSettingsValues::default()),
            draft_session_settings_dirty: RwSignal::new(false),
            font_size: RwSignal::new(13),
            theme: RwSignal::new("dark".to_owned()),
            font_family: RwSignal::new("system".to_owned()),
            syntax_theme: RwSignal::new(crate::syntax_highlight::DEFAULT_THEME_NAME.to_owned()),
            diff_view_mode: RwSignal::new(DiffViewMode::Unified),
            diff_context_mode: RwSignal::new(DiffContextMode::Hunks),
            tool_output_mode: RwSignal::new(ToolOutputMode::Compact),
            custom_agents: RwSignal::new(HashMap::new()),
            mcp_servers: RwSignal::new(HashMap::new()),
            steering: RwSignal::new(HashMap::new()),
            skills: RwSignal::new(HashMap::new()),
            workflow_summaries: RwSignal::new(HashMap::new()),
            workflow_diagnostics: RwSignal::new(HashMap::new()),
            workflow_runs: RwSignal::new(HashMap::new()),
            workflow_locations: RwSignal::new(HashMap::new()),
            workflow_run_request: RwSignal::new(None),
            workflow_command_errors: RwSignal::new(HashMap::new()),
            teams: RwSignal::new(HashMap::new()),
            team_members: RwSignal::new(HashMap::new()),
            team_member_bindings: RwSignal::new(HashMap::new()),
            team_preset_catalogs: RwSignal::new(HashMap::new()),
            team_drafts: RwSignal::new(HashMap::new()),
            team_member_shuffle_suggestions: RwSignal::new(HashMap::new()),
            agents_view_preferences: RwSignal::new(AgentsViewPreferencesSnapshot {
                preferences: AgentsViewPreferences::default(),
                sidebar: Default::default(),
                load_error: None,
                smart_views: Default::default(),
                tags: Default::default(),
                pins: Default::default(),
                groups: Default::default(),
            }),
            agents_view_preferences_host: RwSignal::new(None),
            pending_agents_view_overlay: RwSignal::new(AgentsViewOverlay::default()),
            agents_view_overlay_generation: RwSignal::new(0),
            sessions_panel_filters: RwSignal::new(HashMap::new()),
            reviews: RwSignal::new(HashMap::new()),
            review_summaries: RwSignal::new(HashMap::new()),
            review_create_pending: RwSignal::new(HashMap::new()),
            review_action_pending: RwSignal::new(HashMap::new()),
            review_action_target_pending: RwSignal::new(HashSet::new()),
            compaction_in_progress: RwSignal::new(HashMap::new()),
            compaction_errors: RwSignal::new(HashMap::new()),
            compaction_pending_completion: RwSignal::new(HashMap::new()),
            compaction_pending_close: RwSignal::new(HashSet::new()),
            mobile_access_state: RwSignal::new(HashMap::new()),
            mobile_pairing_offer: RwSignal::new(HashMap::new()),
            mobile_pairing_start_pending: RwSignal::new(HashSet::new()),
            pending_workbench_creates: RwSignal::new(Vec::new()),
            upgrade_attempted: RwSignal::new(HashSet::new()),
        }
    }

    /// Whether the Phase 2 safety net has already fired its one forced
    /// upgrade-and-reconnect for `host_id` on the current connection lifecycle.
    pub fn upgrade_already_attempted(&self, host_id: &str) -> bool {
        self.upgrade_attempted
            .with_untracked(|set| set.contains(host_id))
    }

    /// Record that the one-shot forced upgrade has fired for `host_id`. Blocks a
    /// second auto-upgrade until the guard is cleared (on `Welcome` or explicit
    /// disconnect), so the safety net can never loop.
    pub fn mark_upgrade_attempted(&self, host_id: &str) {
        self.upgrade_attempted.update(|set| {
            set.insert(host_id.to_owned());
        });
    }

    /// Clear the one-shot forced-upgrade guard for `host_id` so a later
    /// legitimate reconnect can attempt the upgrade once more. Called on a
    /// successful `Welcome`; should also be called from the explicit
    /// user-initiated disconnect path.
    pub fn clear_upgrade_attempted(&self, host_id: &str) {
        self.upgrade_attempted.update(|set| {
            set.remove(host_id);
        });
    }

    /// Record that the user has fired a compaction for `(host_id,
    /// agent_id)`. Looks up the agent's `AgentInfo` + team-member
    /// binding so the dispatcher can later correlate the replacement
    /// agent's `NewAgent` echo to this compaction without protocol-
    /// level lineage info. Clears any prior error so a fresh attempt
    /// has a clean error surface.
    pub fn mark_compaction_started(&self, host_id: &str, agent_id: AgentId) {
        self.compaction_errors.update(|m| {
            m.remove(&agent_id);
        });
        let info = self.compaction_info_for(host_id, &agent_id);
        self.compaction_in_progress.update(|map| {
            map.insert(agent_id, info);
        });
    }

    /// Build the fingerprint by reading the agent's own `AgentInfo` and
    /// scanning `team_member_bindings` for any member whose live
    /// `current_agent_id` matches. The team-member id (when present) is
    /// the strongest correlation field because the replacement's
    /// `NewAgent` payload always carries the same value.
    fn compaction_info_for(&self, host_id: &str, agent_id: &AgentId) -> CompactionOldInfo {
        let (project_id, custom_agent_id, backend_kind) = self.agents.with_untracked(|agents| {
            agents
                .iter()
                .find(|a| a.host_id == host_id && &a.agent_id == agent_id)
                .map(|a| {
                    (
                        a.project_id.clone(),
                        a.custom_agent_id.clone(),
                        a.backend_kind,
                    )
                })
                .unwrap_or((None, None, BackendKind::Claude))
        });
        let team_member_id = self.team_member_bindings.with_untracked(|map| {
            map.get(host_id).and_then(|members| {
                members.iter().find_map(|(member_id, binding)| {
                    if binding.current_agent_id.as_ref() == Some(agent_id) {
                        Some(member_id.clone())
                    } else {
                        None
                    }
                })
            })
        });
        CompactionOldInfo {
            host_id: host_id.to_owned(),
            project_id,
            custom_agent_id,
            backend_kind,
            team_member_id,
        }
    }

    /// Find an in-flight compaction whose old-agent fingerprint matches
    /// the new agent identified by `(host_id, fields)`. The dispatcher
    /// uses this in `apply_new_agent` to recognize the replacement and
    /// skip the auto-tab-open path that would otherwise steal focus
    /// from the user's existing chat tab.
    pub fn find_compaction_replacement(
        &self,
        host_id: &str,
        team_member_id: Option<&TeamMemberId>,
        project_id: Option<&ProjectId>,
        custom_agent_id: Option<&CustomAgentId>,
        backend_kind: BackendKind,
    ) -> Option<AgentId> {
        self.compaction_in_progress.with_untracked(|map| {
            for (old_id, info) in map.iter() {
                if info.host_id != host_id {
                    continue;
                }
                // Team-member match is decisive when both sides have a
                // member id: the replacement's NewAgent payload always
                // carries the same one.
                let team_match = match (info.team_member_id.as_ref(), team_member_id) {
                    (Some(a), Some(b)) => a == b,
                    (None, None) => true,
                    _ => false,
                };
                if !team_match {
                    continue;
                }
                if info.project_id.as_ref() != project_id {
                    continue;
                }
                if info.custom_agent_id.as_ref() != custom_agent_id {
                    continue;
                }
                if info.backend_kind != backend_kind {
                    continue;
                }
                return Some(old_id.clone());
            }
            None
        })
    }

    /// Add `(host_id, agent_id)` to the deferred-close set. Used by
    /// `dispatch::apply_agent_closed` when an `AgentClosed` arrives for
    /// an agent that is mid-compaction: we keep the agent's state
    /// alive so `finish_compaction_success` has something to retarget,
    /// and finalize the close from there.
    pub fn defer_compaction_close(&self, host_id: &str, agent_id: AgentId) {
        self.compaction_pending_close.update(|set| {
            set.insert((host_id.to_owned(), agent_id));
        });
    }

    /// Server-confirmed completion: the compaction finished, the
    /// predecessor is being closed, and `new_agent` is the live
    /// replacement. Retargets every chat tab pointing at `prev_agent_id`
    /// to `new_agent` so the user keeps working in the same tab without
    /// remount/focus churn — mirrors `upgrade_pending_team_member_tab`.
    pub fn finish_compaction_success(&self, prev_agent_id: &AgentId, new_agent: &AgentInfo) {
        self.compaction_in_progress.update(|map| {
            map.remove(prev_agent_id);
        });
        self.compaction_errors.update(|m| {
            m.remove(prev_agent_id);
        });
        let new_ref = ActiveAgentRef {
            host_id: new_agent.host_id.clone(),
            agent_id: new_agent.agent_id.clone(),
        };
        let label = new_agent.name.clone();
        let new_ref_for_memory = new_ref.clone();
        let label_for_memory = label.clone();
        let prev_for_cz = prev_agent_id.clone();
        self.center_zone.update(|cz| {
            cz.for_each_tab_mut(|_, tab| {
                if let TabContent::Chat {
                    agent_ref: Some(ar),
                    ..
                } = &tab.content
                    && ar.host_id == new_ref.host_id
                    && ar.agent_id == prev_for_cz
                {
                    tab.content = TabContent::chat_with_agent(new_ref.clone());
                    tab.label = label.clone();
                }
            });
        });
        let prev_for_memory = prev_agent_id.clone();
        self.project_view_memory.update(|map| {
            for memory in map.values_mut() {
                let Some(cz) = memory.center_zone.as_mut() else {
                    continue;
                };
                cz.for_each_tab_mut(|_, tab| {
                    if let TabContent::Chat {
                        agent_ref: Some(ar),
                        ..
                    } = &tab.content
                        && ar.host_id == new_ref_for_memory.host_id
                        && ar.agent_id == prev_for_memory
                    {
                        tab.content = TabContent::chat_with_agent(new_ref_for_memory.clone());
                        tab.label = label_for_memory.clone();
                    }
                });
            }
        });
        // Under the current server contract `AgentClosed` arrives
        // AFTER `Completed`, so the deferred-close set is normally
        // empty here and the cleanup below is a no-op — the normal
        // `apply_agent_closed` path will handle teardown. If the
        // server ever inverts ordering (AgentClosed before Completed),
        // the dispatcher's defer path will have queued the teardown
        // in `compaction_pending_close` and we drain it now, after
        // the retarget, so the old agent's transient state is gone.
        let prev_for_close = prev_agent_id.clone();
        let new_host = new_ref.host_id.clone();
        let had_pending_close = self
            .compaction_pending_close
            .with_untracked(|set| set.contains(&(new_host.clone(), prev_for_close.clone())));
        if had_pending_close {
            self.compaction_pending_close.update(|set| {
                set.remove(&(new_host.clone(), prev_for_close.clone()));
            });
            self.finalize_compaction_close(&new_host, &prev_for_close);
        }
    }

    /// Drop every transient state map entry tied to the closed old
    /// agent. Mirrors `dispatch::apply_agent_closed`'s cleanup set so
    /// the deferred-close path doesn't leave stale entries behind that
    /// the normal close path would have dropped. The tab-related steps
    /// (close any tab still pointing at the old agent + prune LRU) are
    /// belt-and-suspenders here: `finish_compaction_success` retargets
    /// every Chat tab from `old -> new` first, so by the time we reach
    /// this point the close-tabs sweep is typically a no-op. We keep
    /// it because nothing guarantees every surface was retargeted
    /// (e.g. a future tab type that `finish_compaction_success`
    /// doesn't know about), and leaving a stray tab pointing at a
    /// dead agent is worse than a redundant scan.
    /// Drop server-provided prior-history state for a single agent. Call
    /// wherever `chat_rows` is cleared for that agent so a re-bootstrap starts
    /// from the server's new authoritative indicator.
    pub fn forget_session_history(&self, agent_id: &AgentId) {
        self.session_history.update(|map| {
            map.remove(agent_id);
        });
    }

    fn finalize_compaction_close(&self, host_id: &str, agent_id: &AgentId) {
        self.agents.update(|agents| {
            agents.retain(|agent| !(agent.host_id == host_id && agent.agent_id == *agent_id));
        });
        self.chat_rows.update(|map| {
            map.remove(agent_id);
        });
        self.forget_session_history(agent_id);
        self.chat_tool_rows.update(|map| {
            map.remove(agent_id);
        });
        self.tool_progress.update(|map| {
            map.retain(|(id, _), _| id != agent_id);
        });
        self.chat_message_rows.update(|map| {
            map.remove(agent_id);
        });
        self.streaming_text.update(|map| {
            map.remove(agent_id);
        });
        self.agent_activity_stats.update(|map| {
            map.remove(&ActiveAgentRef {
                host_id: host_id.to_owned(),
                agent_id: agent_id.clone(),
            });
        });
        self.task_token_usage.update(|map| {
            map.remove(&ActiveAgentRef {
                host_id: host_id.to_owned(),
                agent_id: agent_id.clone(),
            });
        });
        self.agent_turn_active.update(|map| {
            map.remove(agent_id);
        });
        self.transient_events.update(|map| {
            map.remove(agent_id);
        });
        self.task_lists.update(|map| {
            map.remove(agent_id);
        });
        self.orchestration.update(|map| {
            map.remove(agent_id);
        });
        self.agent_message_queue.update(|map| {
            map.remove(agent_id);
        });
        self.agent_session_settings.update(|map| {
            map.remove(agent_id);
        });
        let host_for_cz = host_id.to_owned();
        let agent_for_cz = agent_id.clone();
        let mut removed_tab_ids = HashSet::new();
        self.center_zone.update(|cz| {
            removed_tab_ids.extend(close_agent_tabs_in_cz(cz, &host_for_cz, &agent_for_cz));
        });
        let host_for_memory = host_id.to_owned();
        let agent_for_memory = agent_id.clone();
        self.project_view_memory.update(|memories| {
            for memory in memories.values_mut() {
                if let Some(center_zone) = memory.center_zone.as_mut() {
                    removed_tab_ids.extend(close_agent_tabs_in_cz(
                        center_zone,
                        &host_for_memory,
                        &agent_for_memory,
                    ));
                }
            }
        });
        self.forget_removed_tab_occurrence_state(&removed_tab_ids);
    }

    /// Server-confirmed failure: the compaction did not produce a
    /// replacement. The predecessor is still alive, so we just clear the
    /// in-flight flag and surface the message on its card. We also
    /// belt-and-suspenders drain the pending-close set in case it ever
    /// gets populated on a failure path.
    pub fn finish_compaction_failure(&self, agent_id: AgentId, message: String) {
        self.compaction_in_progress.update(|map| {
            map.remove(&agent_id);
        });
        let agent_id_for_close = agent_id.clone();
        self.compaction_pending_close.update(|set| {
            set.retain(|(_, a)| a != &agent_id_for_close);
        });
        self.compaction_errors.update(|m| {
            m.insert(agent_id, message);
        });
    }

    pub fn selected_host(&self) -> Option<ConfiguredHost> {
        let selected = self.selected_host_id.get()?;
        self.configured_hosts
            .get()
            .into_iter()
            .find(|host| host.id == selected)
    }

    /// Host that the currently visible chat controls should operate on.
    ///
    /// This intentionally differs from `selected_host_id`, which is the host
    /// selected in Settings. Existing chats are bound to their agent host; new
    /// chats opened while a project is active are bound to that project's host;
    /// only global/Home chats fall back to the Settings-selected host.
    pub fn chat_context_host_id(&self) -> Option<String> {
        if let Some(active_agent) = self.active_agent.get() {
            return Some(active_agent.host_id);
        }
        if let Some(active_project) = self.active_project.get() {
            return Some(active_project.host_id);
        }
        self.selected_host_id.get()
    }

    pub fn chat_context_host_id_untracked(&self) -> Option<String> {
        if let Some(active_agent) = self.active_agent.get_untracked() {
            return Some(active_agent.host_id);
        }
        if let Some(active_project) = self.active_project.get_untracked() {
            return Some(active_project.host_id);
        }
        self.selected_host_id.get_untracked()
    }

    pub fn connection_status_for_host(&self, host_id: &str) -> ConnectionStatus {
        self.connection_statuses
            .get()
            .get(host_id)
            .cloned()
            .unwrap_or(ConnectionStatus::Disconnected)
    }

    pub fn host_settings(&self, host_id: &str) -> Option<HostSettings> {
        self.host_settings_by_host.get().get(host_id).cloned()
    }

    pub fn host_settings_untracked(&self, host_id: &str) -> Option<HostSettings> {
        self.host_settings_by_host
            .get_untracked()
            .get(host_id)
            .cloned()
    }

    pub fn chat_context_connection_status(&self) -> ConnectionStatus {
        let Some(host_id) = self.chat_context_host_id() else {
            return ConnectionStatus::Disconnected;
        };
        self.connection_status_for_host(&host_id)
    }

    pub fn chat_context_host_settings(&self) -> Option<HostSettings> {
        let host_id = self.chat_context_host_id()?;
        self.host_settings(&host_id)
    }

    pub fn chat_context_host_settings_untracked(&self) -> Option<HostSettings> {
        let host_id = self.chat_context_host_id_untracked()?;
        self.host_settings_untracked(&host_id)
    }

    pub fn host_stream_untracked(&self, host_id: &str) -> Option<StreamPath> {
        self.host_streams.get_untracked().get(host_id).cloned()
    }

    pub fn selected_host_stream_untracked(&self) -> Option<(String, StreamPath)> {
        let host_id = self.selected_host_id.get_untracked()?;
        let stream = self.host_stream_untracked(&host_id)?;
        Some((host_id, stream))
    }

    /// Reactively resolve the effective Agents-view preferences: the durable
    /// server snapshot with the non-persisted optimistic overlay layered on top
    /// per preference domain. Reads both signals, so callers inside a reactive
    /// closure re-run when either the server snapshot or the overlay changes.
    pub fn effective_agents_view_preferences(&self) -> AgentsViewPreferences {
        let base = self.agents_view_preferences.get().preferences;
        let overlay = self.pending_agents_view_overlay.get();
        AgentsViewPreferences {
            filters: overlay.filters.unwrap_or(base.filters),
            sort_mode: overlay.sort_mode.unwrap_or(base.sort_mode),
            group_mode: overlay.group_mode.unwrap_or(base.group_mode),
            density: overlay.density.unwrap_or(base.density),
            hide_finished: overlay.hide_finished.unwrap_or(base.hide_finished),
            manual_order: overlay.manual_order.unwrap_or(base.manual_order),
        }
    }

    /// Reactively resolve the effective sidebar selector preferences (hide
    /// inactive / hide sub-agents / project visibility): the durable server
    /// snapshot with the optimistic overlay layered on top. Reads both signals,
    /// so callers inside a reactive closure re-run when either changes.
    pub fn effective_agents_sidebar_preferences(&self) -> AgentsSidebarPreferences {
        self.pending_agents_view_overlay
            .get()
            .sidebar
            .unwrap_or_else(|| self.agents_view_preferences.get().sidebar)
    }

    /// Reactively resolve the active Smart View id: the optimistic overlay
    /// value when a view selection (or a divergent query edit) is in flight,
    /// otherwise the server snapshot's `active_view_id`. `None` means no view
    /// is highlighted — either the server reports a custom/divergent query or
    /// an in-flight edit cleared the highlight. Reads both signals so callers
    /// inside a reactive closure re-run on either change.
    pub fn effective_active_smart_view_id(&self) -> Option<SmartViewId> {
        match self.pending_agents_view_overlay.get().active_view_id {
            Some(active) => active,
            None => {
                self.agents_view_preferences
                    .get()
                    .smart_views
                    .active_view_id
            }
        }
    }

    /// Apply a server-emitted Agents-view preference snapshot. Only the primary
    /// local host owns these preferences (dev-docs/26 §12.1): a `Some` snapshot
    /// from any other host is ignored so a stray remote payload cannot hijack
    /// the client-global signal or its owner pointer.
    ///
    /// The snapshot is authoritative and full, so the optimistic overlay is
    /// dropped wholesale — the server wins even when its canonicalized value
    /// differs from the optimistic one (sorted filter enums, retained historical
    /// session keys). Matching the optimistic value exactly is impossible after
    /// canonicalization, so an equality-only reconcile would leave the overlay
    /// stuck and mask future server changes to that domain.
    pub fn apply_agents_view_snapshot(
        &self,
        host_id: &str,
        snapshot: AgentsViewPreferencesSnapshot,
    ) {
        if host_id != PRIMARY_LOCAL_HOST_ID {
            log::warn!("ignoring agents-view preferences snapshot from non-primary host {host_id}");
            return;
        }
        self.agents_view_preferences.set(snapshot);
        self.agents_view_preferences_host
            .set(Some(host_id.to_owned()));
        // A new authoritative snapshot supersedes every in-flight domain. Bump
        // the generation so any pending safety-timeout for the old overlay
        // becomes a no-op.
        self.agents_view_overlay_generation
            .update(|generation| *generation = generation.wrapping_add(1));
        self.pending_agents_view_overlay
            .set(AgentsViewOverlay::default());
    }

    /// Install an optimistic overlay update for an in-flight preference domain
    /// and run `mutate` on the overlay. Used right before a
    /// `SetAgentsViewPreferences` frame is sent so the UI reacts immediately.
    /// Arms a safety timeout so a dropped/failed send cannot freeze the view.
    pub fn set_agents_view_overlay(&self, mutate: impl FnOnce(&mut AgentsViewOverlay)) {
        self.pending_agents_view_overlay
            .update(|overlay| mutate(overlay));
        let generation = self
            .agents_view_overlay_generation
            .try_update(|generation| {
                *generation = generation.wrapping_add(1);
                *generation
            })
            .unwrap_or_default();
        self.arm_overlay_reconcile_timeout(generation);
    }

    /// Schedule the safety backstop: after `OVERLAY_RECONCILE_TIMEOUT_MS`, if no
    /// newer overlay mutation or authoritative snapshot has bumped the
    /// generation and the overlay is still pending, drop it. Uses `try_*`
    /// accessors so a timer that fires after the owning scope is disposed (e.g.
    /// across test boundaries) is a harmless no-op. No-op off wasm.
    #[cfg(target_arch = "wasm32")]
    fn arm_overlay_reconcile_timeout(&self, generation: u64) {
        use wasm_bindgen::JsCast;
        use wasm_bindgen::closure::Closure;

        let state = self.clone();
        let callback = Closure::once_into_js(move || {
            let still_current = state
                .agents_view_overlay_generation
                .try_get_untracked()
                .map(|current| current == generation)
                .unwrap_or(false);
            if !still_current {
                return;
            }
            let _ = state.pending_agents_view_overlay.try_update(|overlay| {
                if !overlay.is_empty() {
                    log::warn!("agents-view overlay timed out without server reconcile; dropping");
                    *overlay = AgentsViewOverlay::default();
                }
            });
        });
        if let Some(window) = web_sys::window() {
            let _ = window.set_timeout_with_callback_and_timeout_and_arguments_0(
                callback.unchecked_ref(),
                OVERLAY_RECONCILE_TIMEOUT_MS,
            );
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn arm_overlay_reconcile_timeout(&self, _generation: u64) {}

    /// True while any preference mutation is awaiting server confirmation.
    pub fn agents_view_overlay_pending(&self) -> bool {
        !self.pending_agents_view_overlay.get().is_empty()
    }

    pub fn push_chat_entry(&self, agent_id: AgentId, entry: ChatMessageEntry) -> ChatRowHandle {
        let handle = ChatRowHandle::new(entry);
        let (indexed_tool_call_ids, message_id) = handle.entry.with_untracked(|entry| {
            (
                entry
                    .tool_requests
                    .iter()
                    .map(|tool| tool.request.tool_call_id.clone())
                    .collect::<Vec<_>>(),
                entry.message.message_id.clone(),
            )
        });

        self.chat_rows.update(|rows| {
            let agent_rows = rows.entry(agent_id.clone()).or_default();
            agent_rows.push(handle.clone());
        });

        if !indexed_tool_call_ids.is_empty() {
            self.chat_tool_rows.update(|indexes| {
                let agent_index = indexes.entry(agent_id.clone()).or_default();
                for tool_call_id in indexed_tool_call_ids {
                    agent_index.insert(ToolCallId(tool_call_id), handle.id);
                }
            });
        }

        if let Some(message_id) = message_id {
            self.chat_message_rows.update(|indexes| {
                indexes
                    .entry(agent_id)
                    .or_default()
                    .entry(message_id)
                    .or_insert(handle.id);
            });
        }

        handle
    }

    /// Patch the row matching `update.message_id` with whichever of
    /// `model_info` / `token_usage` / `context_breakdown` are `Some`. A
    /// `None` update field means "leave the existing value alone" — this
    /// is a partial update, not a replace. Unknown message ids are
    /// logged and otherwise ignored: server-side guarantees that the
    /// `MessageMetadataUpdated` for a Codex turn arrives after the
    /// visible `StreamEnd` that created the row, but if the row was
    /// dropped (compaction, agent close) by the time the update lands
    /// we just want to no-op, not crash.
    pub fn apply_chat_message_metadata(
        &self,
        agent_id: &AgentId,
        update: MessageMetadataUpdateData,
    ) {
        let row_id = self.chat_message_rows.with_untracked(|indexes| {
            indexes
                .get(agent_id)
                .and_then(|agent_index| agent_index.get(&update.message_id).copied())
        });
        let Some(row_id) = row_id else {
            log::warn!(
                "chat_event message_metadata_updated unknown message_id agent_id={} message_id={}",
                agent_id,
                update.message_id
            );
            return;
        };
        let Some(handle) = self.chat_row_by_id_untracked(agent_id, row_id) else {
            log::warn!(
                "chat_event message_metadata_updated row gone agent_id={} message_id={} row_id={:?}",
                agent_id,
                update.message_id,
                row_id
            );
            return;
        };
        let row_message_id = handle
            .entry
            .with_untracked(|entry| entry.message.message_id.clone());
        if row_message_id.as_ref() != Some(&update.message_id) {
            log::warn!(
                "chat_event message_metadata_updated stale row agent_id={} expected_message_id={} row_message_id={:?} row_id={:?}",
                agent_id,
                update.message_id,
                row_message_id,
                row_id
            );
            return;
        }
        handle.entry.update(|entry| {
            if let Some(model_info) = update.model_info {
                entry.message.model_info = Some(model_info);
            }
            if let Some(token_usage) = update.token_usage {
                entry.message.token_usage = Some(token_usage);
            }
            if let Some(context_breakdown) = update.context_breakdown {
                entry.message.context_breakdown = Some(context_breakdown);
            }
        });
    }

    pub fn last_chat_row_untracked(&self, agent_id: &AgentId) -> Option<ChatRowHandle> {
        self.chat_rows
            .with_untracked(|rows| rows.get(agent_id).and_then(|rows| rows.last().cloned()))
    }

    pub fn chat_row_by_id_untracked(
        &self,
        agent_id: &AgentId,
        row_id: ChatRowId,
    ) -> Option<ChatRowHandle> {
        self.chat_rows.with_untracked(|rows| {
            rows.get(agent_id)
                .and_then(|rows| rows.iter().find(|row| row.id == row_id).cloned())
        })
    }

    pub fn index_chat_tool_row(&self, agent_id: &AgentId, tool_call_id: String, row_id: ChatRowId) {
        self.chat_tool_rows.update(|indexes| {
            indexes
                .entry(agent_id.clone())
                .or_default()
                .insert(ToolCallId(tool_call_id), row_id);
        });
    }

    pub fn chat_row_for_tool_untracked(
        &self,
        agent_id: &AgentId,
        tool_call_id: &str,
    ) -> Option<ChatRowHandle> {
        let row_id = self.chat_tool_rows.with_untracked(|indexes| {
            indexes.get(agent_id).and_then(|agent_index| {
                agent_index
                    .get(&ToolCallId(tool_call_id.to_owned()))
                    .copied()
            })
        })?;
        self.chat_row_by_id_untracked(agent_id, row_id)
    }

    pub fn selected_host_settings(&self) -> Option<HostSettings> {
        let host_id = self.selected_host_id.get()?;
        self.host_settings_by_host.get().get(&host_id).cloned()
    }

    pub fn selected_host_settings_untracked(&self) -> Option<HostSettings> {
        let host_id = self.selected_host_id.get_untracked()?;
        self.host_settings_by_host
            .get_untracked()
            .get(&host_id)
            .cloned()
    }

    pub fn selected_host_backend_setup(&self) -> Option<Vec<BackendSetupInfo>> {
        let host_id = self.selected_host_id.get()?;
        self.backend_setup_by_host.get().get(&host_id).cloned()
    }

    pub fn selected_host_connection_status(&self) -> ConnectionStatus {
        let Some(host_id) = self.selected_host_id.get() else {
            return ConnectionStatus::Disconnected;
        };
        self.connection_statuses
            .get()
            .get(&host_id)
            .cloned()
            .unwrap_or(ConnectionStatus::Disconnected)
    }

    pub fn selected_host_command_error(&self) -> Option<String> {
        let host_id = self.selected_host_id.get()?;
        self.command_errors_by_host.get().get(&host_id).cloned()
    }

    /// Apply a server-emitted Add-report shuffle suggestion notify. Each
    /// notify bumps a per-(host, team) serial so the open dialog can
    /// detect fresh suggestions without re-applying stale ones.
    pub fn record_team_member_shuffle_suggestion(
        &self,
        host_id: &str,
        payload: TeamMemberShuffleSuggestionNotifyPayload,
    ) {
        let TeamMemberShuffleSuggestionNotifyPayload {
            team_id,
            suggestion,
        } = payload;
        self.team_member_shuffle_suggestions.update(|map| {
            let host_map = map.entry(host_id.to_owned()).or_default();
            let previous_serial = host_map
                .get(&team_id)
                .map(|entry| entry.serial)
                .unwrap_or(0);
            host_map.insert(
                team_id,
                TeamMemberShuffleSuggestionEntry {
                    suggestion,
                    serial: previous_serial.saturating_add(1),
                },
            );
        });
    }

    pub fn active_project_ref_untracked(&self) -> Option<ActiveProjectRef> {
        self.active_project.get_untracked()
    }

    pub fn queue_pending_agent_session_settings(
        &self,
        host_id: String,
        project_id: Option<ProjectId>,
        values: SessionSettingsValues,
    ) -> u64 {
        let id = self.next_pending_agent_session_settings_id.get_untracked();
        self.next_pending_agent_session_settings_id
            .set(id.wrapping_add(1));
        self.pending_agent_session_settings.update(|pending| {
            pending
                .entry((host_id, project_id))
                .or_default()
                .push_back(PendingAgentSessionSettings { id, values });
        });
        id
    }

    pub fn discard_pending_agent_session_settings(
        &self,
        host_id: &str,
        project_id: Option<&ProjectId>,
        id: u64,
    ) {
        let key = (host_id.to_owned(), project_id.cloned());
        self.pending_agent_session_settings.update(|pending| {
            let remove_key = if let Some(queue) = pending.get_mut(&key) {
                queue.retain(|entry| entry.id != id);
                queue.is_empty()
            } else {
                false
            };
            if remove_key {
                pending.remove(&key);
            }
        });
    }

    pub fn take_pending_agent_session_settings(
        &self,
        host_id: &str,
        project_id: Option<&ProjectId>,
    ) -> Option<SessionSettingsValues> {
        let key = (host_id.to_owned(), project_id.cloned());
        self.pending_agent_session_settings
            .try_update(|pending| {
                let queue = pending.get_mut(&key)?;
                let entry = queue.pop_front()?;
                if queue.is_empty() {
                    pending.remove(&key);
                }
                Some(entry.values)
            })
            .flatten()
    }

    pub fn restore_active_project_after_host_bootstrap(&self, host_id: &str) {
        let Some(pending) = self
            .pending_active_project_restore
            .get_untracked()
            .filter(|pending| pending.host_id == host_id)
        else {
            return;
        };
        self.pending_active_project_restore.set(None);

        if self.active_project.get_untracked().is_some() {
            return;
        }
        let exists = self.projects.with_untracked(|projects| {
            projects.iter().any(|project| {
                project.host_id == pending.host_id && project.project.id == pending.project_id
            })
        });
        if exists {
            self.switch_active_project(Some(pending));
        } else {
            persist_active_project(None);
        }
    }

    /// Whether the project at `(host_id, project_id)` accepts ProjectAddRoot /
    /// ProjectDeleteRoot. Per §6.5/§6.6 of the workbenches design doc:
    ///
    /// - A workbench's roots are managed only by WorkbenchCreate /
    ///   WorkbenchRemove — root edits are rejected with `InvalidInput`.
    /// - A standalone parent that has at least one workbench child is
    ///   rejected with `Conflict` because root edits would break the
    ///   parent_root linkage in every child workbench.
    /// - Otherwise (standalone with no children), root edits are allowed.
    ///
    /// The UI mirrors this: hide / disable add-root and per-root remove
    /// affordances when the answer is `false`. The server is still the
    /// enforcement boundary; this is just a projection of state.
    pub fn can_manage_project_roots(&self, host_id: &str, project_id: &ProjectId) -> bool {
        let projects = self.projects.get();
        let Some(project) = projects
            .iter()
            .find(|info| info.host_id == host_id && &info.project.id == project_id)
        else {
            return false;
        };
        if project.project.is_workbench() {
            return false;
        }
        let has_workbench_children = projects.iter().any(|info| {
            info.host_id == host_id && info.project.parent_project_id() == Some(project_id)
        });
        !has_workbench_children
    }

    /// Change which project the center zone is viewing. Snapshots the outgoing
    /// project's center-zone state into `project_view_memory` and restores the
    /// incoming project's last snapshot (or a fresh empty Chat view for a
    /// project seen for the first time, or Home view when switching to none).
    pub fn switch_active_project(&self, next: Option<ActiveProjectRef>) {
        let current = self.active_project.get_untracked();
        if current == next {
            return;
        }

        if let Some(outgoing) = current {
            let snapshot = ProjectViewMemory {
                center_zone: Some(self.center_zone.get_untracked()),
                active_terminal: self.active_terminal.get_untracked(),
                open_files: self.open_files.get_untracked(),
                diff_contents: self.diff_contents.get_untracked(),
            };
            self.project_view_memory.update(|map| {
                map.insert(outgoing, snapshot);
            });
        }

        let restored = next.as_ref().and_then(|r| {
            self.project_view_memory
                .with_untracked(|m| m.get(r).cloned())
        });

        self.active_project.set(next.clone());
        persist_active_project(next.as_ref());

        // Notify the host that this project became active so the server can warm
        // code intelligence and restore recent history. This is the one central
        // switch path, so every selection route (rail click, resume, team open,
        // new-chat prefill) is covered. Home/None never notifies. Duplicate
        // sends on switch are fine — the server owns idempotency.
        #[cfg(target_arch = "wasm32")]
        if let Some(accessed) = next.as_ref() {
            let host_id = accessed.host_id.clone();
            let stream = StreamPath(format!("/project/{}", accessed.project_id.0));
            wasm_bindgen_futures::spawn_local(async move {
                if let Err(error) = crate::send::project_accessed(&host_id, stream).await {
                    log::error!("failed to send ProjectAccessed: {error}");
                }
            });
        }

        // active_agent is a Memo derived from center_zone — restoring center_zone
        // implicitly restores it. Tab LRU is reset and re-seeded with the
        // incoming project's active tab so the first render after switch
        // mounts content (avoids a one-frame empty flash before the Effect
        // in `App` fires).
        self.references_state
            .set(ProjectReferencesUiState::default());
        self.code_intel_hover.set(None);
        self.code_intel_navigate_ctx.set(None);
        self.pending_file_opens.set(HashMap::new());

        match (next.is_some(), restored) {
            (true, Some(memory)) => {
                let cz = memory.center_zone.unwrap_or_default();
                self.tab_lru.set(
                    cz.panes()
                        .filter_map(|(_, pane)| pane.active_tab_id)
                        .collect(),
                );
                if let Some(ratio) = cz.split_ratio() {
                    self.center_split_ratio.set(ratio);
                }
                self.center_zone.set(cz);
                self.active_terminal.set(memory.active_terminal);
                self.open_files.set(memory.open_files);
                self.diff_contents.set(memory.diff_contents);
            }
            (true, None) => {
                let mut cz = CenterZoneState::default();
                cz.open(TabContent::empty_chat(), "New Chat".to_string(), true);
                self.tab_lru.set(
                    cz.panes()
                        .filter_map(|(_, pane)| pane.active_tab_id)
                        .collect(),
                );
                self.center_zone.set(cz);
                self.active_terminal.set(None);
                self.open_files.set(HashMap::new());
                self.diff_contents.set(HashMap::new());
            }
            (false, _) => {
                let cz = CenterZoneState::default();
                self.tab_lru.set(
                    cz.panes()
                        .filter_map(|(_, pane)| pane.active_tab_id)
                        .collect(),
                );
                self.center_zone.set(cz);
                self.active_terminal.set(None);
                self.open_files.set(HashMap::new());
                self.diff_contents.set(HashMap::new());
            }
        }
    }

    pub fn forget_project_view_memory(&self, project: &ActiveProjectRef) {
        let mut removed_tab_ids = HashSet::new();
        self.project_view_memory.update(|map| {
            if let Some(memory) = map.remove(project)
                && let Some(center_zone) = memory.center_zone
            {
                removed_tab_ids.extend(center_zone.all_tab_ids());
            }
        });
        self.forget_removed_tab_occurrence_state(&removed_tab_ids);
    }

    pub fn active_project_info_untracked(&self) -> Option<ProjectInfo> {
        let active = self.active_project.get_untracked()?;
        self.projects.get_untracked().into_iter().find(|project| {
            project.host_id == active.host_id && project.project.id == active.project_id
        })
    }

    pub fn active_connection_count(&self) -> usize {
        self.connection_statuses
            .get()
            .values()
            .filter(|status| matches!(status, ConnectionStatus::Connected))
            .count()
    }

    pub fn total_host_count(&self) -> usize {
        self.configured_hosts.get().len()
    }

    pub fn clear_host_runtime(&self, host_id: &str) {
        self.pending_agent_session_settings
            .update(|pending| pending.retain(|(pending_host, _), _| pending_host != host_id));
        let host_project_ids: HashSet<ProjectId> = self.projects.with_untracked(|projects| {
            projects
                .iter()
                .filter(|project| project.host_id == host_id)
                .map(|project| project.project.id.clone())
                .collect()
        });
        let active_project_on_host = self
            .active_project
            .get_untracked()
            .as_ref()
            .is_some_and(|active| active.host_id == host_id);
        if active_project_on_host {
            self.switch_active_project(None);
        }

        let reviews_before = self.reviews.with_untracked(|m| m.len());
        let action_gates_before = self.review_action_pending.with_untracked(|m| m.len());
        let target_gates_before = self
            .review_action_target_pending
            .with_untracked(|s| s.len());
        let create_pending_before = self
            .review_create_pending
            .with_untracked(|m| m.iter().filter(|((h, _), _)| h == host_id).count());
        log::info!(
            "host.clear_host_runtime.start host={host_id} host_projects={} reviews_before={reviews_before} action_gates_before={action_gates_before} target_gates_before={target_gates_before} host_create_pending={create_pending_before}",
            host_project_ids.len()
        );
        // Drop chat-related per-agent state for every agent on this host before
        // we forget the agent list itself. Without this, a reconnect re-replays
        // every event and the dispatcher appends duplicate messages onto the
        // already-cached vectors.
        let agent_ids: Vec<AgentId> = self.agents.with_untracked(|agents| {
            agents
                .iter()
                .filter(|agent| agent.host_id == host_id)
                .map(|agent| agent.agent_id.clone())
                .collect()
        });
        let drop_set: HashSet<AgentId> = agent_ids.iter().cloned().collect();
        let mut removed_tab_ids = HashSet::new();
        self.center_zone.update(|center_zone| {
            removed_tab_ids.extend(close_host_runtime_tabs_in_cz(center_zone, host_id));
        });
        self.project_view_memory.update(|memories| {
            for (project, memory) in memories.iter_mut() {
                if let Some(center_zone) = memory.center_zone.as_mut() {
                    if project.host_id == host_id {
                        removed_tab_ids.extend(center_zone.all_tab_ids());
                    } else {
                        removed_tab_ids.extend(close_host_runtime_tabs_in_cz(center_zone, host_id));
                    }
                }
                if memory
                    .active_terminal
                    .as_ref()
                    .is_some_and(|active| active.host_id == host_id)
                {
                    memory.active_terminal = None;
                }
                memory.diff_contents.retain(|key, _| key.host_id != host_id);
            }
        });
        self.forget_removed_tab_occurrence_state(&removed_tab_ids);
        if !drop_set.is_empty() {
            self.chat_rows.update(|map| {
                map.retain(|id, _| !drop_set.contains(id));
            });
            self.chat_tool_rows.update(|map| {
                map.retain(|id, _| !drop_set.contains(id));
            });
            self.tool_progress.update(|map| {
                map.retain(|(id, _), _| !drop_set.contains(id));
            });
            self.chat_message_rows.update(|map| {
                map.retain(|id, _| !drop_set.contains(id));
            });
            self.streaming_text.update(|map| {
                map.retain(|id, _| !drop_set.contains(id));
            });
            self.agent_activity_stats.update(|map| {
                map.retain(|key, _| key.host_id != host_id);
            });
            self.task_token_usage.update(|map| {
                map.retain(|key, _| key.host_id != host_id);
            });
            self.session_history.update(|map| {
                map.retain(|id, _| !drop_set.contains(id));
            });
            self.task_lists.update(|map| {
                map.retain(|id, _| !drop_set.contains(id));
            });
            self.orchestration.update(|map| {
                map.retain(|id, _| !drop_set.contains(id));
            });
            self.transient_events.update(|map| {
                map.retain(|id, _| !drop_set.contains(id));
            });
            self.agent_message_queue.update(|map| {
                map.retain(|id, _| !drop_set.contains(id));
            });
            self.agent_turn_active.update(|map| {
                map.retain(|id, _| !drop_set.contains(id));
            });
            self.agent_session_settings.update(|map| {
                map.retain(|id, _| !drop_set.contains(id));
            });
        }

        let compaction_ids: HashSet<AgentId> = self.compaction_in_progress.with_untracked(|map| {
            map.iter()
                .filter(|(_, info)| info.host_id == host_id)
                .map(|(agent_id, _)| agent_id.clone())
                .collect()
        });
        let mut compaction_drop_set = drop_set.clone();
        compaction_drop_set.extend(compaction_ids);
        self.compaction_in_progress.update(|map| {
            map.retain(|_, info| info.host_id != host_id);
        });
        self.compaction_errors.update(|map| {
            map.retain(|id, _| !compaction_drop_set.contains(id));
        });
        self.compaction_pending_completion.update(|map| {
            map.retain(|(host, _), _| host != host_id);
        });
        self.compaction_pending_close.update(|set| {
            set.retain(|(host, _)| host != host_id);
        });

        let review_ids: HashSet<ReviewId> = self.reviews.with_untracked(|reviews| {
            reviews
                .iter()
                .filter(|(_, review)| host_project_ids.contains(&review.project_id))
                .map(|(review_id, _)| review_id.clone())
                .collect()
        });
        let summary_review_ids: HashSet<ReviewId> =
            self.review_summaries.with_untracked(|summaries| {
                summaries
                    .iter()
                    .filter(|(project_id, _)| host_project_ids.contains(project_id))
                    .flat_map(|(_, summaries)| summaries.iter().map(|summary| summary.id.clone()))
                    .collect()
            });
        let mut host_review_ids = review_ids;
        host_review_ids.extend(summary_review_ids);

        self.file_tree.update(|map| {
            map.retain(|project_id, _| !host_project_ids.contains(project_id));
        });
        self.git_status.update(|map| {
            map.retain(|project_id, _| !host_project_ids.contains(project_id));
        });
        self.code_intel_overview.update(|map| {
            map.retain(|key, _| key.host_id != host_id);
        });
        self.code_intel.update(|map| {
            map.retain(|key, _| key.host_id != host_id);
        });
        self.open_files.update(|map| {
            map.retain(|key, _| key.host_id != host_id);
        });
        self.pending_file_opens.update(|map| {
            map.retain(|key, _| key.host_id != host_id);
        });
        self.diff_contents.update(|map| {
            map.retain(|key, _| key.host_id != host_id);
        });
        self.code_intel_navigate_ctx.update(|ctx| {
            if ctx.as_ref().is_some_and(|ctx| ctx.key.host_id == host_id) {
                *ctx = None;
            }
        });
        self.code_intel_hover.update(|hover| {
            if hover
                .as_ref()
                .is_some_and(|hover| hover.key.host_id == host_id)
            {
                *hover = None;
            }
        });
        self.references_state.update(|references| {
            if references
                .source_key
                .as_ref()
                .is_some_and(|key| key.host_id == host_id)
            {
                *references = ProjectReferencesUiState::default();
            }
        });
        self.review_summaries.update(|map| {
            map.retain(|project_id, _| !host_project_ids.contains(project_id));
        });
        self.reviews.update(|map| {
            map.retain(|_, review| !host_project_ids.contains(&review.project_id));
        });
        self.review_action_pending.update(|map| {
            map.retain(|review_id, _| !host_review_ids.contains(review_id));
        });
        self.review_action_target_pending.update(|set| {
            set.retain(|(review_id, _)| !host_review_ids.contains(review_id));
        });
        self.review_create_pending.update(|map| {
            map.retain(|(host, _), _| host != host_id);
        });
        // NOTE: Agents-view preferences (manual order + filters) are
        // intentionally NOT pruned here. They are server-owned durable state
        // replayed on the next bootstrap; pruning them on host cleanup is
        // exactly the behavior that produced the Agents-tab flicker/reset. The
        // non-persisted optimistic overlay is likewise left untouched — it is
        // reconciled by the next server notify/bootstrap, never by host
        // teardown. See `dev-docs/26-agent-organization.md` §5.5.
        self.sessions_panel_filters.update(|map| {
            map.retain(|active, _| {
                active
                    .as_ref()
                    .is_none_or(|active| active.host_id != host_id)
            });
        });

        self.host_streams.update(|streams| {
            streams.remove(host_id);
        });
        // Drop per-host validator state on both directions. Otherwise a
        // reconnect keeps stale seq/protocol stream state, but the server
        // builds fresh validators per connection and replays bootstraps from
        // seq 0.
        crate::send::clear_host_seqs(host_id);
        crate::dispatch::reset_inbound_state_for_host(host_id);
        self.command_errors_by_host.update(|errors| {
            errors.remove(host_id);
        });
        self.host_lifecycle_statuses.update(|statuses| {
            statuses.remove(host_id);
        });
        self.host_settings_by_host.update(|settings| {
            settings.remove(host_id);
        });
        self.backend_setup_by_host.update(|setup| {
            setup.remove(host_id);
        });
        self.mobile_access_state.update(|map| {
            map.remove(host_id);
        });
        self.mobile_pairing_offer.update(|map| {
            map.remove(host_id);
        });
        self.mobile_pairing_start_pending.update(|set| {
            set.remove(host_id);
        });
        self.session_schemas.update(|schemas| {
            schemas.remove(host_id);
        });
        self.backend_config_schemas.update(|schemas| {
            schemas.remove(host_id);
        });
        self.backend_config_snapshots.update(|snapshots| {
            snapshots.remove(host_id);
        });
        // Capacity is never carried across a connection. A rehydrated figure
        // from a previous session would render as `Known` while being
        // arbitrarily old — quota moves, and a stale-but-confident number is
        // worse than an honest absence. The server replays the current snapshot
        // on the next subscribe.
        self.backend_capacity.update(|snapshots| {
            snapshots.remove(host_id);
        });
        self.backend_native_settings.update(|snapshots| {
            snapshots.remove(host_id);
        });
        self.native_settings_save_state.update(|states| {
            states.remove(host_id);
        });
        self.schemas_loaded_for_host.update(|loaded| {
            loaded.remove(host_id);
        });
        self.launch_profile_catalog.update(|map| {
            map.remove(host_id);
        });
        self.custom_agents.update(|map| {
            map.remove(host_id);
        });
        self.mcp_servers.update(|map| {
            map.remove(host_id);
        });
        self.steering.update(|map| {
            map.remove(host_id);
        });
        self.skills.update(|map| {
            map.remove(host_id);
        });
        self.workflow_summaries.update(|map| {
            map.remove(host_id);
        });
        self.workflow_diagnostics.update(|map| {
            map.remove(host_id);
        });
        self.workflow_runs.update(|map| {
            map.remove(host_id);
        });
        self.workflow_locations.update(|map| {
            map.remove(host_id);
        });
        self.teams.update(|map| {
            map.remove(host_id);
        });
        self.team_members.update(|map| {
            map.remove(host_id);
        });
        self.team_member_bindings.update(|map| {
            map.remove(host_id);
        });
        self.team_preset_catalogs.update(|map| {
            map.remove(host_id);
        });
        self.team_drafts.update(|map| {
            map.remove(host_id);
        });
        self.team_member_shuffle_suggestions.update(|map| {
            map.remove(host_id);
        });
        self.projects
            .update(|projects| projects.retain(|project| project.host_id != host_id));
        self.agents
            .update(|agents| agents.retain(|agent| agent.host_id != host_id));
        self.sessions
            .update(|sessions| sessions.retain(|session| session.host_id != host_id));
        self.terminals
            .update(|terminals| terminals.retain(|terminal| terminal.host_id != host_id));
        self.project_view_memory
            .update(|map| map.retain(|key, _| key.host_id != host_id));
        self.pending_workbench_creates
            .update(|pending| pending.retain(|entry| entry.host_id != host_id));
        self.pending_terminal_focus.update(|focus| {
            if focus.as_deref() == Some(host_id) {
                *focus = None;
            }
        });
        self.browse_dialog.update(|dialog| {
            if dialog
                .as_ref()
                .is_some_and(|dialog| dialog.host_id.get_untracked() == host_id)
            {
                *dialog = None;
            }
        });
        if self
            .active_terminal
            .get_untracked()
            .as_ref()
            .is_some_and(|active| active.host_id == host_id)
        {
            self.active_terminal.set(None);
        }
    }

    // ── Tab convenience methods ─────────────────────────────────────────

    pub fn open_tab(&self, content: TabContent, label: String, closeable: bool) {
        let target = self
            .center_zone
            .with_untracked(|center_zone| center_zone.resolve(OpenTarget::Focused));
        self.open_tab_in(target, content, label, closeable);
    }

    #[cfg(all(test, target_arch = "wasm32"))]
    pub fn open_tab_at(
        &self,
        target: OpenTarget,
        content: TabContent,
        label: String,
        closeable: bool,
    ) -> Option<TabId> {
        let pane = self
            .center_zone
            .with_untracked(|center_zone| center_zone.resolve(target));
        self.open_tab_in(pane, content, label, closeable)
    }

    pub fn open_tab_in(
        &self,
        pane: PaneId,
        content: TabContent,
        label: String,
        closeable: bool,
    ) -> Option<TabId> {
        let tabs_enabled = self.tabs_enabled.get_untracked();
        let ratio = self.center_split_ratio.get_untracked();
        let mut result = None;
        let mut replaced_id = None;
        self.center_zone.update(|center_zone| {
            if let Some(existing) = center_zone.find_tab_in(pane, &content) {
                center_zone.activate(existing);
                result = Some(existing);
                return;
            }
            if let Some(existing) = center_zone.find_tab(&content) {
                center_zone.activate(existing);
                result = Some(existing);
                return;
            }
            if tabs_enabled {
                result = Some(center_zone.open_in(pane, content, label, closeable, ratio));
            } else {
                let id = center_zone.replace_active(content, label, closeable);
                replaced_id = Some(id);
                result = Some(id);
            }
        });
        if let Some(id) = replaced_id {
            self.forget_tab_scroll_state(id);
        }
        result
    }

    pub fn set_split_ratio(&self, ratio: SplitRatio) {
        self.center_split_ratio.set(ratio);
        self.center_zone
            .update(|center_zone| center_zone.set_split_ratio(ratio));
    }

    pub fn focus_pane(&self, pane: PaneId) -> bool {
        let active = self
            .center_zone
            .with_untracked(|center_zone| center_zone.pane_active_tab_id(pane));
        let Some(active) = active else {
            return false;
        };
        self.activate_tab(active);
        true
    }

    #[cfg(test)]
    pub fn duplicate_file_eligibility_at(
        &self,
        target: OpenTarget,
        source: TabId,
    ) -> DuplicateFileEligibility {
        let target_pane = self
            .center_zone
            .with(|center_zone| center_zone.resolve(target));
        self.duplicate_file_eligibility_in(target_pane, source)
    }

    #[cfg(test)]
    pub fn duplicate_file_eligibility_in(
        &self,
        target: PaneId,
        source: TabId,
    ) -> DuplicateFileEligibility {
        let tabs_enabled = self.tabs_enabled.get();
        self.center_zone.with(|center_zone| {
            self.open_files.with(|open_files| {
                duplicate_file_eligibility_for(
                    tabs_enabled,
                    center_zone,
                    open_files,
                    target,
                    source,
                )
            })
        })
    }

    #[cfg(test)]
    pub fn duplicate_file_at_result(
        &self,
        target: OpenTarget,
        source: TabId,
    ) -> DuplicateFileResult {
        let target_pane = self
            .center_zone
            .with_untracked(|center_zone| center_zone.resolve(target));
        self.duplicate_file_in_result(target_pane, source)
    }

    pub fn duplicate_file_in_result(&self, target: PaneId, source: TabId) -> DuplicateFileResult {
        let eligibility = {
            let tabs_enabled = self.tabs_enabled.get_untracked();
            self.center_zone.with_untracked(|center_zone| {
                self.open_files.with_untracked(|open_files| {
                    duplicate_file_eligibility_for(
                        tabs_enabled,
                        center_zone,
                        open_files,
                        target,
                        source,
                    )
                })
            })
        };
        match eligibility {
            DuplicateFileEligibility::Enabled => {}
            DuplicateFileEligibility::TargetAlreadyContainsResource { existing } => {
                self.reveal_tab(existing);
                return DuplicateFileResult::ActivatedExisting {
                    source,
                    existing,
                    target,
                };
            }
            DuplicateFileEligibility::TabsDisabled => {
                return DuplicateFileResult::TabsDisabled;
            }
            DuplicateFileEligibility::SourceTabMissing => {
                return DuplicateFileResult::SourceTabMissing;
            }
            DuplicateFileEligibility::NotAFile => return DuplicateFileResult::NotAFile,
            DuplicateFileEligibility::NotLoaded => return DuplicateFileResult::NotLoaded,
        }

        let ratio = self.center_split_ratio.get_untracked();
        let mut result = None;
        self.center_zone.update(|center_zone| {
            result = center_zone.duplicate_file_to(source, target, ratio);
        });
        match result {
            Some(tab) => DuplicateFileResult::Duplicated {
                source,
                tab,
                target,
            },
            None => DuplicateFileResult::SourceTabMissing,
        }
    }

    pub fn move_tab_eligibility(&self, target: PaneId, id: TabId) -> MoveTabEligibility {
        self.center_zone
            .with_untracked(|center_zone| center_zone.move_tab_eligibility(target, id))
    }

    pub fn move_tab_to(&self, target: PaneId, id: TabId) -> MoveTabResult {
        match self.move_tab_eligibility(target, id) {
            MoveTabEligibility::Eligible => {}
            MoveTabEligibility::SourceTabMissing => return MoveTabResult::SourceTabMissing,
            MoveTabEligibility::AlreadyInTargetPane => {
                return MoveTabResult::AlreadyInTargetPane;
            }
            MoveTabEligibility::ResourceAlreadyInTarget { existing } => {
                return MoveTabResult::ResourceAlreadyInTarget { existing };
            }
        }
        let ratio = self.center_split_ratio.get_untracked();
        let mut result = MoveTabResult::SourceTabMissing;
        self.center_zone.update(|center_zone| {
            result = center_zone.move_tab_to(target, id, ratio);
        });
        result
    }

    pub fn split_tab_to(&self, target: PaneId, id: TabId) -> MoveTabResult {
        let ratio = self.center_split_ratio.get_untracked();
        let mut result = MoveTabResult::SourceTabMissing;
        self.center_zone.update(|center_zone| {
            result = center_zone.split_tab_to(target, id, ratio);
        });
        result
    }

    /// Returns `None` when the agent chat can open to the side, or the exact
    /// typed refusal the activation API would return from the same state.
    /// Signal reads are tracked so render-time disabled state stays reactive.
    #[cfg(test)]
    pub fn agent_open_to_side_eligibility(
        &self,
        agent_ref: &ActiveAgentRef,
        project: Option<&ActiveProjectRef>,
    ) -> Option<AgentOpenToSideResult> {
        let tabs_enabled = self.tabs_enabled.get();
        let active_project = self.active_project.get();
        self.center_zone.with(|center_zone| {
            agent_open_to_side_block_for(
                tabs_enabled,
                active_project.as_ref(),
                center_zone,
                agent_ref,
                project,
            )
        })
    }

    #[cfg(test)]
    pub fn open_agent_chat_to_side(
        &self,
        agent_ref: ActiveAgentRef,
        project: Option<ActiveProjectRef>,
        label: String,
    ) -> AgentOpenToSideResult {
        let blocked = {
            let tabs_enabled = self.tabs_enabled.get_untracked();
            let active_project = self.active_project.get_untracked();
            self.center_zone.with_untracked(|center_zone| {
                agent_open_to_side_block_for(
                    tabs_enabled,
                    active_project.as_ref(),
                    center_zone,
                    &agent_ref,
                    project.as_ref(),
                )
            })
        };
        if let Some(blocked) = blocked {
            return blocked;
        }

        let content = TabContent::chat_with_agent(agent_ref);
        let (focused, existing) = self.center_zone.with_untracked(|center_zone| {
            let focused = center_zone.focused_id();
            let existing = center_zone
                .find_tab(&content)
                .and_then(|tab| center_zone.locate_tab(tab).map(|pane| (pane, tab)));
            (focused, existing)
        });
        let target = focused.other();

        if let Some((pane, tab)) = existing {
            if pane == target {
                self.reveal_tab(tab);
                return AgentOpenToSideResult::Revealed { tab, pane };
            }
            let source_retains_content = self.center_zone.with_untracked(|center_zone| {
                center_zone
                    .pane(pane)
                    .is_some_and(|source| source.tabs.iter().any(|candidate| candidate.id != tab))
            });
            if !source_retains_content {
                return AgentOpenToSideResult::NothingWouldRemain;
            }
            return match self.move_tab_to(target, tab) {
                MoveTabResult::Moved { source, target, .. } => AgentOpenToSideResult::Moved {
                    tab,
                    source,
                    target,
                },
                MoveTabResult::SourceTabMissing => {
                    AgentOpenToSideResult::MoveRefused(MoveTabRefusal::SourceTabMissing)
                }
                MoveTabResult::AlreadyInTargetPane => {
                    AgentOpenToSideResult::MoveRefused(MoveTabRefusal::AlreadyInTargetPane)
                }
                MoveTabResult::ResourceAlreadyInTarget { existing } => {
                    AgentOpenToSideResult::MoveRefused(MoveTabRefusal::ResourceAlreadyInTarget {
                        existing,
                    })
                }
            };
        }

        let Some(tab) = self.open_tab_in(target, content, label, true) else {
            return AgentOpenToSideResult::NothingWouldRemain;
        };
        AgentOpenToSideResult::Opened { tab, pane: target }
    }

    /// Returns `None` when the exact diff can open to the side, or the typed
    /// refusal the activation API would return from the same state.
    #[cfg(test)]
    pub fn diff_open_to_side_eligibility(&self, key: &DiffKey) -> Option<DiffOpenToSideResult> {
        let tabs_enabled = self.tabs_enabled.get();
        let active_project = self.active_project.get();
        self.center_zone.with(|center_zone| {
            diff_open_to_side_block_for(tabs_enabled, active_project.as_ref(), center_zone, key)
        })
    }

    #[cfg(test)]
    pub fn open_diff_to_side(&self, key: DiffKey, label: String) -> DiffOpenToSideResult {
        let blocked = {
            let tabs_enabled = self.tabs_enabled.get_untracked();
            let active_project = self.active_project.get_untracked();
            self.center_zone.with_untracked(|center_zone| {
                diff_open_to_side_block_for(
                    tabs_enabled,
                    active_project.as_ref(),
                    center_zone,
                    &key,
                )
            })
        };
        if let Some(blocked) = blocked {
            return blocked;
        }

        let content = key.tab_content();
        let (focused, existing) = self.center_zone.with_untracked(|center_zone| {
            let focused = center_zone.focused_id();
            let existing = center_zone
                .find_tab(&content)
                .and_then(|tab| center_zone.locate_tab(tab).map(|pane| (pane, tab)));
            (focused, existing)
        });
        let target = focused.other();

        if let Some((pane, tab)) = existing {
            if pane == target {
                self.reveal_tab(tab);
                return DiffOpenToSideResult::Revealed { tab, pane };
            }
            let source_retains_content = self.center_zone.with_untracked(|center_zone| {
                center_zone
                    .pane(pane)
                    .is_some_and(|source| source.tabs.iter().any(|candidate| candidate.id != tab))
            });
            if !source_retains_content {
                return DiffOpenToSideResult::NothingWouldRemain;
            }
            return match self.move_tab_to(target, tab) {
                MoveTabResult::Moved { source, target, .. } => DiffOpenToSideResult::Moved {
                    tab,
                    source,
                    target,
                },
                MoveTabResult::SourceTabMissing => {
                    DiffOpenToSideResult::MoveRefused(MoveTabRefusal::SourceTabMissing)
                }
                MoveTabResult::AlreadyInTargetPane => {
                    DiffOpenToSideResult::MoveRefused(MoveTabRefusal::AlreadyInTargetPane)
                }
                MoveTabResult::ResourceAlreadyInTarget { existing } => {
                    DiffOpenToSideResult::MoveRefused(MoveTabRefusal::ResourceAlreadyInTarget {
                        existing,
                    })
                }
            };
        }

        let Some(tab) = self.open_tab_in(target, content, label, true) else {
            return DiffOpenToSideResult::NothingWouldRemain;
        };
        DiffOpenToSideResult::Opened { tab, pane: target }
    }

    pub fn file_occurrence_in(&self, pane: PaneId, key: &FileResourceKey) -> Option<TabId> {
        self.center_zone.with_untracked(|center_zone| {
            center_zone.find_tab_in(pane, &TabContent::File { key: key.clone() })
        })
    }

    pub fn resolve_file_occurrence(
        &self,
        key: &FileResourceKey,
        preferred: PaneId,
    ) -> Option<(PaneId, TabId)> {
        self.center_zone.with_untracked(|center_zone| {
            center_zone
                .find_tab_in(preferred, &TabContent::File { key: key.clone() })
                .map(|tab| (preferred, tab))
                .or_else(|| {
                    center_zone
                        .find_tab_in(preferred.other(), &TabContent::File { key: key.clone() })
                        .map(|tab| (preferred.other(), tab))
                })
        })
    }

    pub fn file_occurrence_is_current(
        &self,
        tab: TabId,
        key: &FileResourceKey,
        version: ProjectFileVersion,
    ) -> bool {
        let occurrence_matches = self.center_zone.with_untracked(|center_zone| {
            center_zone.tab(tab).is_some_and(|candidate| {
                matches!(&candidate.content, TabContent::File { key: candidate_key } if candidate_key == key)
            })
        });
        occurrence_matches
            && self
                .open_files
                .with_untracked(|files| files.get(key).is_some_and(|file| file.version == version))
    }

    pub fn target_file_navigation(&self, tab: TabId, navigation: PendingFileNavigation) {
        match navigation {
            PendingFileNavigation::Line(line) => self.pending_goto_line.set(Some((tab, line))),
            PendingFileNavigation::Offset(offset) => {
                self.pending_goto_offset.set(Some((tab, offset)))
            }
        }
    }

    pub fn record_pending_file_open(&self, key: FileResourceKey, intent: PendingFileOpen) {
        self.pending_file_opens.update(|pending| match intent {
            PendingFileOpen::Open { .. } => {
                pending.insert(key, intent);
            }
            PendingFileOpen::RefreshInPlace => {
                if !matches!(pending.get(&key), Some(PendingFileOpen::Open { .. })) {
                    pending.insert(key, intent);
                }
            }
        });
    }

    pub fn take_pending_file_open(&self, key: &FileResourceKey) -> Option<PendingFileOpen> {
        let mut intent = None;
        self.pending_file_opens.update(|pending| {
            intent = pending.remove(key);
        });
        intent
    }

    /// Insert `id` at the MRU front of `tab_lru`, dedup, truncate to
    /// `TAB_LRU_CAPACITY`. Visible pane actives are pinned separately by
    /// `mounted_tab_ids`, so switching in one pane cannot evict the other.
    pub fn bump_tab_lru(&self, id: TabId) {
        self.tab_lru.update(|lru| {
            lru.retain(|existing| *existing != id);
            lru.insert(0, id);
            if lru.len() > TAB_LRU_CAPACITY {
                lru.truncate(TAB_LRU_CAPACITY);
            }
        });
    }

    pub fn mounted_tab_ids(&self) -> Vec<TabId> {
        let lru = self.tab_lru.get();
        self.center_zone.with(|center_zone| {
            let pinned: HashSet<TabId> = center_zone
                .panes()
                .filter_map(|(_, pane)| pane.active_tab_id)
                .collect();
            center_zone
                .all_tabs()
                .filter(|(_, tab)| pinned.contains(&tab.id) || lru.contains(&tab.id))
                .map(|(_, tab)| tab.id)
                .collect()
        })
    }

    #[cfg(test)]
    pub fn forget_tab_lru(&self, id: TabId) {
        self.tab_lru.update(|lru| {
            lru.retain(|existing| *existing != id);
        });
    }

    pub fn tab_scroll_state_untracked(&self, id: TabId) -> Option<TabScrollState> {
        self.tab_scroll_state
            .with_untracked(|scroll| scroll.get(&id).copied())
    }

    pub fn save_tab_scroll_state(&self, id: TabId, scroll_state: TabScrollState) {
        self.tab_scroll_state.update(|scroll| {
            scroll.insert(id, scroll_state);
        });
    }

    pub fn forget_tab_scroll_state(&self, id: TabId) {
        self.tab_scroll_state.update(|scroll| {
            scroll.remove(&id);
        });
    }

    pub(crate) fn forget_removed_tab_occurrence_state(&self, doomed: &HashSet<TabId>) {
        if doomed.is_empty() {
            return;
        }
        self.tab_lru.update(|lru| {
            lru.retain(|id| !doomed.contains(id));
        });
        self.tab_scroll_state.update(|scroll| {
            scroll.retain(|id, _| !doomed.contains(id));
        });
        self.code_intel_hover.update(|hover| {
            if hover
                .as_ref()
                .is_some_and(|hover| doomed.contains(&hover.tab))
            {
                *hover = None;
            }
        });
        self.code_intel_navigate_ctx.update(|context| {
            if context
                .as_ref()
                .is_some_and(|context| doomed.contains(&context.tab))
            {
                *context = None;
            }
        });
    }

    #[cfg(test)]
    pub fn prune_tab_lru(&self) {
        let live = self
            .center_zone
            .with_untracked(CenterZoneState::all_tab_ids);
        self.tab_lru.update(|lru| {
            lru.retain(|id| live.contains(id));
        });
    }

    pub fn backing_release_projection(
        &self,
        doomed: &HashSet<TabId>,
    ) -> (HashSet<BackingResource>, HashSet<BackingResource>) {
        self.center_zone.with_untracked(|center_zone| {
            let survivors: HashSet<BackingResource> = center_zone
                .all_tabs()
                .filter(|(_, tab)| !doomed.contains(&tab.id))
                .filter_map(|(_, tab)| tab.backing_resource())
                .collect();
            let released = center_zone
                .all_tabs()
                .filter(|(_, tab)| doomed.contains(&tab.id))
                .filter_map(|(_, tab)| tab.backing_resource())
                .filter(|resource| !survivors.contains(resource))
                .collect();
            (survivors, released)
        })
    }

    fn tear_down_backing_resource(&self, resource: &BackingResource) {
        match resource {
            BackingResource::File(key) => {
                self.open_files.update(|files| {
                    files.remove(key);
                });
                self.pending_file_opens.update(|pending| {
                    pending.remove(key);
                });
                self.drop_code_intel(key);
            }
            BackingResource::Diff(key) => {
                self.diff_contents.update(|diffs| {
                    diffs.remove(key);
                });
            }
        }
    }

    /// Dismiss the hover popover and supersede any in-flight hover request so
    /// its late result is dropped (mirrors `actions::dismiss_hover`, which
    /// delegates here; the logic lives on state so tab-activation paths can
    /// dismiss without depending on the actions layer).
    pub fn dismiss_code_intel_hover(&self) {
        let mut id = 0;
        self.code_intel_request_seq.update(|seq| {
            *seq = seq.wrapping_add(1).max(1);
            id = *seq;
        });
        self.code_intel_active_hover.set(id);
        if self
            .code_intel_hover
            .with_untracked(|hover| hover.is_some())
        {
            self.code_intel_hover.set(None);
        }
    }

    fn drop_code_intel(&self, file: &FileResourceKey) {
        let key = CodeIntelKey {
            host_id: file.host_id.clone(),
            project_id: file.project_id.clone(),
            path: file.path.clone(),
        };
        self.code_intel.update(|map| {
            map.remove(&key);
        });
        #[cfg(target_arch = "wasm32")]
        {
            let host_id = file.host_id.clone();
            let stream = StreamPath(format!("/project/{}", file.project_id.0));
            let payload = protocol::CodeIntelUnsubscribeFilePayload {
                path: file.path.clone(),
            };
            wasm_bindgen_futures::spawn_local(async move {
                if let Err(error) = crate::send::send_frame(
                    &host_id,
                    stream,
                    protocol::FrameKind::CodeIntelUnsubscribeFile,
                    &payload,
                )
                .await
                {
                    log::error!("failed to send CodeIntelUnsubscribeFile: {error}");
                }
            });
        }
    }

    pub fn close_tabs(&self, doomed: HashSet<TabId>) {
        if doomed.is_empty() {
            return;
        }
        let (_, released) = self.backing_release_projection(&doomed);
        for resource in &released {
            self.tear_down_backing_resource(resource);
        }
        self.forget_removed_tab_occurrence_state(&doomed);
        self.center_zone
            .update(|center_zone| center_zone.remove_tabs(&doomed));
    }

    pub fn close_tab(&self, id: TabId) {
        let closeable = self
            .center_zone
            .with_untracked(|center_zone| center_zone.tab(id).is_some_and(|tab| tab.closeable));
        if closeable {
            self.close_tabs(HashSet::from([id]));
        }
    }

    pub fn reveal_tab(&self, id: TabId) -> bool {
        let mut revealed = false;
        self.center_zone.update(|center_zone| {
            revealed = center_zone.reveal_tab(id);
        });
        // A hover popover is pinned to viewport coordinates captured over the
        // previously visible content; it has no meaning over another tab. Any
        // tab activation (mouse, keyboard, or programmatic) dismisses it and
        // supersedes an in-flight hover request.
        if revealed {
            self.dismiss_code_intel_hover();
        }
        revealed
    }

    #[cfg(test)]
    pub fn set_active_tab_in_pane(&self, pane: PaneId, id: TabId) -> bool {
        let mut selected = false;
        self.center_zone.update(|center_zone| {
            selected = center_zone.set_active_tab_in_pane(pane, id);
        });
        selected
    }

    pub fn update_tab(&self, id: TabId, content: TabContent, label: String) -> bool {
        let mut updated = false;
        self.center_zone.update(|center_zone| {
            updated = center_zone.update_tab(id, content, label);
        });
        updated
    }

    pub fn activate_tab(&self, id: TabId) {
        self.reveal_tab(id);
    }

    pub fn close_other_tabs(&self, id: TabId) {
        let doomed = self.center_zone.with_untracked(|center_zone| {
            let Some(pane_id) = center_zone.locate_tab(id) else {
                return HashSet::new();
            };
            center_zone
                .pane(pane_id)
                .into_iter()
                .flat_map(|pane| pane.tabs.iter())
                .filter(|tab| tab.id != id && tab.closeable)
                .map(|tab| tab.id)
                .collect()
        });
        self.close_tabs(doomed);
        self.activate_tab(id);
    }

    pub fn close_tabs_to_right(&self, id: TabId) {
        let doomed = self.center_zone.with_untracked(|center_zone| {
            let Some(pane_id) = center_zone.locate_tab(id) else {
                return HashSet::new();
            };
            let Some(pane) = center_zone.pane(pane_id) else {
                return HashSet::new();
            };
            let Some(index) = pane.tabs.iter().position(|tab| tab.id == id) else {
                return HashSet::new();
            };
            pane.tabs[index + 1..]
                .iter()
                .filter(|tab| tab.closeable)
                .map(|tab| tab.id)
                .collect()
        });
        self.close_tabs(doomed);
    }

    pub fn close_all_tabs(&self) {
        let doomed = self.center_zone.with_untracked(|center_zone| {
            center_zone
                .all_tabs()
                .filter(|(_, tab)| tab.closeable)
                .map(|(_, tab)| tab.id)
                .collect()
        });
        self.close_tabs(doomed);
    }

    pub fn close_pane(&self, pane: PaneId) {
        let doomed = self.center_zone.with_untracked(|center_zone| {
            if !center_zone.is_split() {
                return HashSet::new();
            }
            center_zone
                .pane(pane)
                .into_iter()
                .flat_map(|pane| pane.tabs.iter())
                .map(|tab| tab.id)
                .collect()
        });
        self.close_tabs(doomed);
    }

    pub fn close_other_pane(&self) {
        let other = self
            .center_zone
            .with_untracked(|center_zone| center_zone.focused_id().other());
        self.close_pane(other);
    }

    pub fn rename_tab_label(&self, id: TabId, new_label: String) {
        self.center_zone
            .update(|center_zone| center_zone.rename_tab_label(id, new_label));
    }

    pub fn composer_pending_team_member_untracked(&self) -> Option<PendingTeamMember> {
        self.center_zone.with_untracked(|center_zone| {
            let (_, tab_id) = center_zone.composer_owner()?;
            center_zone.tab(tab_id).and_then(|tab| match &tab.content {
                TabContent::Chat {
                    agent_ref: None,
                    pending_team_member: Some(pending),
                } => Some(pending.clone()),
                _ => None,
            })
        })
    }
}

#[cfg(test)]
mod code_intel_tests {
    use super::*;
    use protocol::{
        ByteRange, CodeIntelCompleteness, CodeIntelLanguageId, CodeIntelLocation,
        CodeIntelModelRange, CodeIntelOccurrence, CodeIntelProviderId, CodeIntelResourceMode,
        CodeIntelRole, CodeIntelState, CodeIntelStatusScope,
    };

    fn path() -> ProjectPath {
        ProjectPath {
            root: ProjectRootPath("/repo".to_owned()),
            relative_path: "src/main.rs".to_owned(),
        }
    }

    fn status(version: ProjectFileVersion, state: CodeIntelState) -> CodeIntelStatusPayload {
        CodeIntelStatusPayload {
            scope: CodeIntelStatusScope::File {
                path: path(),
                version,
            },
            state,
            resource_mode: CodeIntelResourceMode::Full,
            work_done: None,
            total_work: None,
            message: None,
        }
    }

    fn model(version: ProjectFileVersion) -> CodeIntelFileModelPayload {
        CodeIntelFileModelPayload {
            path: path(),
            version,
            provider: CodeIntelProviderId("mock".to_owned()),
            language: CodeIntelLanguageId("rust".to_owned()),
            model_range: CodeIntelModelRange::FullFile,
            completeness: CodeIntelCompleteness::Complete,
            occurrences: Vec::new(),
        }
    }

    #[test]
    fn frame_at_rendered_version_is_applied() {
        let mut s = CodeIntelFileState::default();
        s.set_rendered_version(ProjectFileVersion(5));
        s.merge_versioned(ProjectFileVersion(5), |d| {
            d.status = Some(status(ProjectFileVersion(5), CodeIntelState::Ready));
        });
        let applied = s.applied().expect("data applied at rendered version");
        assert_eq!(
            applied.status.as_ref().map(|st| st.state),
            Some(CodeIntelState::Ready)
        );
    }

    #[test]
    fn older_frame_is_dropped() {
        let mut s = CodeIntelFileState::default();
        s.set_rendered_version(ProjectFileVersion(5));
        s.merge_versioned(ProjectFileVersion(4), |d| {
            d.model = Some(model(ProjectFileVersion(4)));
        });
        // Nothing applied (v4 dropped), and no v4 entry stashed.
        assert!(s.applied().is_none());
        assert!(s.by_version.is_empty());
    }

    #[test]
    fn newer_frame_is_stashed_then_applied_when_contents_arrive() {
        let mut s = CodeIntelFileState::default();
        s.set_rendered_version(ProjectFileVersion(5));
        // A v6 model arrives before the v6 contents: must not paint over v5.
        s.merge_versioned(ProjectFileVersion(6), |d| {
            d.model = Some(model(ProjectFileVersion(6)));
        });
        assert!(s.applied().is_none(), "v6 must not apply over v5 text");
        assert!(s.by_version.contains_key(&ProjectFileVersion(6)));

        // v6 contents land: the stashed v6 model is now the applied data, and
        // the stale v5 entry is dropped.
        s.set_rendered_version(ProjectFileVersion(6));
        let applied = s.applied().expect("v6 data applied once contents arrive");
        assert!(applied.model.is_some());
        assert!(!s.by_version.contains_key(&ProjectFileVersion(5)));
    }

    #[test]
    fn frame_before_any_contents_is_stashed() {
        let mut s = CodeIntelFileState::default();
        // No rendered version yet (contents not arrived).
        s.merge_versioned(ProjectFileVersion(1), |d| {
            d.status = Some(status(ProjectFileVersion(1), CodeIntelState::Indexing));
        });
        assert!(s.applied().is_none());
        s.set_rendered_version(ProjectFileVersion(1));
        assert_eq!(
            s.applied()
                .and_then(|d| d.status.as_ref())
                .map(|st| st.state),
            Some(CodeIntelState::Indexing)
        );
    }

    #[test]
    fn pre_content_version_stash_is_bounded() {
        let mut s = CodeIntelFileState::default();
        for version in 1..=10 {
            s.merge_versioned(ProjectFileVersion(version), |d| {
                d.status = Some(status(
                    ProjectFileVersion(version),
                    CodeIntelState::Indexing,
                ));
            });
        }
        assert_eq!(s.by_version.len(), CODE_INTEL_PRE_CONTENT_STASH_LIMIT);
        assert!(
            !s.by_version.contains_key(&ProjectFileVersion(1)),
            "oldest pre-content versions are dropped once the stash is capped"
        );
        assert!(s.by_version.contains_key(&ProjectFileVersion(10)));
    }

    #[test]
    fn version_change_drops_stale_decorations_and_ignores_late_old_frames() {
        // §M4 external-change correctness: a file rendered at v5 with applied
        // decorations reloads to v6. The stale v5 decorations must be dropped
        // (not painted over v6 text), a late v5 frame arriving *after* the bump
        // must be ignored, and a v6 frame applies cleanly.
        let mut s = CodeIntelFileState::default();
        s.set_rendered_version(ProjectFileVersion(5));
        s.merge_versioned(ProjectFileVersion(5), |d| {
            d.model = Some(model(ProjectFileVersion(5)));
            d.diagnostics = vec![diagnostic()];
        });
        assert!(s.applied().expect("v5 applied").model.is_some());

        // v6 contents arrive (the reload): v5 decorations are dropped.
        s.set_rendered_version(ProjectFileVersion(6));
        assert!(!s.by_version.contains_key(&ProjectFileVersion(5)));
        assert!(
            s.applied().is_none(),
            "no v6 frame yet ⇒ nothing applied (never the stale v5 data)"
        );

        // A late v5 frame (in-flight before the bump) is dropped, not stashed.
        s.merge_versioned(ProjectFileVersion(5), |d| {
            d.diagnostics = vec![diagnostic()];
        });
        assert!(!s.by_version.contains_key(&ProjectFileVersion(5)));

        // The fresh v6 model applies at the new rendered version.
        s.merge_versioned(ProjectFileVersion(6), |d| {
            d.model = Some(model(ProjectFileVersion(6)));
        });
        let applied = s.applied().expect("v6 applied after reload");
        assert_eq!(
            applied.model.as_ref().map(|m| m.version),
            Some(ProjectFileVersion(6))
        );
    }

    fn diagnostic() -> CodeIntelDiagnostic {
        CodeIntelDiagnostic {
            range: ByteRange { start: 0, end: 1 },
            severity: protocol::CodeIntelSeverity::Error,
            message: "boom".to_owned(),
            source: None,
        }
    }

    #[test]
    fn diagnostics_at_returns_hits_under_offset_most_severe_first() {
        let mut s = CodeIntelFileState::default();
        s.set_rendered_version(ProjectFileVersion(1));
        s.merge_versioned(ProjectFileVersion(1), |d| {
            d.diagnostics = vec![
                CodeIntelDiagnostic {
                    range: ByteRange { start: 4, end: 10 },
                    severity: protocol::CodeIntelSeverity::Hint,
                    message: "consider removing".to_owned(),
                    source: None,
                },
                CodeIntelDiagnostic {
                    range: ByteRange { start: 4, end: 10 },
                    severity: protocol::CodeIntelSeverity::Error,
                    message: "mismatched types".to_owned(),
                    source: Some("rustc".to_owned()),
                },
                CodeIntelDiagnostic {
                    range: ByteRange { start: 20, end: 24 },
                    severity: protocol::CodeIntelSeverity::Warning,
                    message: "elsewhere".to_owned(),
                    source: None,
                },
            ];
        });

        let hits = s.diagnostics_at(ProjectFileVersion(1), 5);
        assert_eq!(hits.len(), 2, "only diagnostics containing the offset");
        assert_eq!(hits[0].message, "mismatched types", "most severe first");
        assert_eq!(hits[1].message, "consider removing");

        assert!(s.diagnostics_at(ProjectFileVersion(1), 15).is_empty());
        // End is exclusive.
        assert!(s.diagnostics_at(ProjectFileVersion(1), 10).is_empty());
        // A mismatched version never paints another version's diagnostics.
        assert!(s.diagnostics_at(ProjectFileVersion(2), 5).is_empty());
    }

    #[test]
    fn diagnostics_at_matches_zero_width_ranges_at_their_anchor() {
        let mut s = CodeIntelFileState::default();
        s.set_rendered_version(ProjectFileVersion(1));
        s.merge_versioned(ProjectFileVersion(1), |d| {
            d.diagnostics = vec![CodeIntelDiagnostic {
                range: ByteRange { start: 7, end: 7 },
                severity: protocol::CodeIntelSeverity::Error,
                message: "expected `;`".to_owned(),
                source: None,
            }];
        });
        assert_eq!(s.diagnostics_at(ProjectFileVersion(1), 7).len(), 1);
        assert!(s.diagnostics_at(ProjectFileVersion(1), 6).is_empty());
        assert!(s.diagnostics_at(ProjectFileVersion(1), 8).is_empty());
    }

    fn occurrence(start: u32, end: u32, definition: Vec<CodeIntelLocation>) -> CodeIntelOccurrence {
        CodeIntelOccurrence {
            range: ByteRange { start, end },
            role: CodeIntelRole::Reference,
            display: "sym".to_owned(),
            definition,
        }
    }

    #[test]
    fn merge_model_merges_occurrences_by_range() {
        let mut data = CodeIntelData::default();

        // First frame: two bare occurrences (no targets yet), Partial.
        let mut first = model(ProjectFileVersion(2));
        first.completeness = CodeIntelCompleteness::Partial;
        first.occurrences = vec![occurrence(0, 3, vec![]), occurrence(10, 13, vec![])];
        data.merge_model(first);

        // Second frame at the same version: fills in the target for the [0,3)
        // occurrence and adds a new occurrence; marks the model Complete.
        let target = CodeIntelLocation {
            path: path(),
            range: ByteRange {
                start: 99,
                end: 102,
            },
        };
        let mut second = model(ProjectFileVersion(2));
        second.completeness = CodeIntelCompleteness::Complete;
        second.occurrences = vec![
            occurrence(0, 3, vec![target.clone()]),
            occurrence(20, 23, vec![]),
        ];
        data.merge_model(second);

        // Third frame: re-sends [0,3) with an EMPTY definition (e.g. a fresh
        // semanticTokens pass before its definition re-resolves). The already-
        // resolved target MUST survive — this is the incremental-streaming
        // invariant that M3 relies on.
        let mut third = model(ProjectFileVersion(2));
        third.occurrences = vec![occurrence(0, 3, vec![])];
        data.merge_model(third);

        let merged = data.model.expect("model present after merge");
        // [0,3) updated in place (target filled), [10,13) preserved, [20,23) added.
        assert_eq!(merged.occurrences.len(), 3);
        let zero = merged
            .occurrences
            .iter()
            .find(|o| o.range == ByteRange { start: 0, end: 3 })
            .expect("[0,3) occurrence retained by range");
        assert_eq!(
            zero.definition,
            vec![target],
            "resolved target survives a later same-range frame with an empty definition"
        );
        assert!(
            merged
                .occurrences
                .iter()
                .any(|o| o.range == ByteRange { start: 10, end: 13 }),
            "untouched occurrence is preserved, not wiped by the second frame"
        );
        // Latest frame's completeness wins.
        assert_eq!(merged.completeness, CodeIntelCompleteness::Complete);
    }

    #[test]
    fn merge_accumulates_byte_range_chunks_then_completes() {
        // §M6: a large file streams transient `ByteRange` + `Partial` chunks
        // (visible window first) converging on a final `FullFile` + `Complete`
        // frame. The client must accumulate occurrences across chunks and only
        // flip to "complete" on the Complete frame — ByteRange is a pacing
        // window, never a permanent scope gate.
        let mut data = CodeIntelData::default();

        // Visible chunk first: occurrence [20,23) with its target resolved.
        let resolved = CodeIntelLocation {
            path: path(),
            range: ByteRange {
                start: 99,
                end: 102,
            },
        };
        let mut visible_chunk = model(ProjectFileVersion(7));
        visible_chunk.model_range = CodeIntelModelRange::ByteRange {
            range: ByteRange { start: 20, end: 23 },
        };
        visible_chunk.completeness = CodeIntelCompleteness::Partial;
        visible_chunk.occurrences = vec![occurrence(20, 23, vec![resolved.clone()])];
        data.merge_model(visible_chunk);
        // A ByteRange Partial chunk must NOT read as complete.
        assert_eq!(
            data.model.as_ref().unwrap().completeness,
            CodeIntelCompleteness::Partial,
            "a ByteRange chunk is never complete on its own"
        );

        // Offscreen chunk next: a different byte window, still Partial.
        let mut offscreen_chunk = model(ProjectFileVersion(7));
        offscreen_chunk.model_range = CodeIntelModelRange::ByteRange {
            range: ByteRange { start: 0, end: 3 },
        };
        offscreen_chunk.completeness = CodeIntelCompleteness::Partial;
        offscreen_chunk.occurrences = vec![occurrence(0, 3, vec![])];
        data.merge_model(offscreen_chunk);
        assert_eq!(
            data.model.as_ref().unwrap().completeness,
            CodeIntelCompleteness::Partial,
            "still streaming → still Partial after a second chunk"
        );
        assert_eq!(
            data.model.as_ref().unwrap().occurrences.len(),
            2,
            "chunks accumulate by range — coverage grows, nothing dropped"
        );

        // Final FullFile + Complete marker: flips the whole-file coverage signal.
        let mut complete = model(ProjectFileVersion(7));
        complete.model_range = CodeIntelModelRange::FullFile;
        complete.completeness = CodeIntelCompleteness::Complete;
        complete.occurrences = Vec::new();
        data.merge_model(complete);

        let merged = data.model.expect("model present");
        assert_eq!(
            merged.completeness,
            CodeIntelCompleteness::Complete,
            "coverage only flips to complete on the Complete frame"
        );
        assert_eq!(
            merged.model_range,
            CodeIntelModelRange::FullFile,
            "the model converges to whole-file scope"
        );
        // Both chunks' occurrences survive the Complete marker, and the visible
        // chunk's resolved target is preserved.
        assert_eq!(merged.occurrences.len(), 2);
        let visible = merged
            .occurrences
            .iter()
            .find(|o| o.range == ByteRange { start: 20, end: 23 })
            .expect("visible occurrence retained");
        assert_eq!(visible.definition, vec![resolved]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_tab(content: TabContent, label: &str, closeable: bool) -> Tab {
        Tab {
            id: next_tab_id(),
            content,
            label: label.to_string(),
            closeable,
        }
    }

    #[test]
    fn upgrade_guard_starts_absent_then_marks_and_clears() {
        let owner = leptos::reactive::owner::Owner::new();
        owner.with(|| {
            let state = AppState::new();
            let host = "managed-host";

            assert!(!state.upgrade_already_attempted(host));
            state.mark_upgrade_attempted(host);
            assert!(state.upgrade_already_attempted(host));
            state.clear_upgrade_attempted(host);
            assert!(!state.upgrade_already_attempted(host));
        });
    }

    #[test]
    fn upgrade_guard_clear_of_absent_id_is_noop() {
        let owner = leptos::reactive::owner::Owner::new();
        owner.with(|| {
            let state = AppState::new();
            // Clearing an id that was never marked must not panic or flip any
            // other state — it is simply a no-op.
            state.clear_upgrade_attempted("never-marked");
            assert!(!state.upgrade_already_attempted("never-marked"));
        });
    }

    #[test]
    fn upgrade_guard_is_independent_per_host() {
        let owner = leptos::reactive::owner::Owner::new();
        owner.with(|| {
            let state = AppState::new();
            let host_a = "host-a";
            let host_b = "host-b";

            state.mark_upgrade_attempted(host_a);
            assert!(state.upgrade_already_attempted(host_a));
            assert!(!state.upgrade_already_attempted(host_b));

            // Clearing one host leaves the other untouched.
            state.clear_upgrade_attempted(host_a);
            assert!(!state.upgrade_already_attempted(host_a));

            state.mark_upgrade_attempted(host_b);
            assert!(state.upgrade_already_attempted(host_b));
            assert!(!state.upgrade_already_attempted(host_a));
        });
    }

    #[test]
    fn close_others_keeps_target_and_non_closeable() {
        let home = make_tab(TabContent::Home, "Home", false);
        let chat1 = make_tab(TabContent::empty_chat(), "Chat 1", true);
        let chat2 = make_tab(TabContent::empty_chat(), "Chat 2", true);
        let target_id = chat1.id;
        let mut cz = CenterZoneState {
            layout: CenterLayout::Single(PaneState {
                tabs: vec![home, chat1, chat2],
                active_tab_id: None,
            }),
        };
        cz.close_others(target_id);
        assert_eq!(cz.tabs().len(), 2);
        assert!(cz.tabs().iter().any(|t| t.id == target_id));
        assert!(cz.tabs().iter().any(|t| !t.closeable));
        assert_eq!(cz.active_tab_id(), Some(target_id));
    }

    #[test]
    fn close_to_right_removes_closeable_tabs_after_target() {
        let home = make_tab(TabContent::Home, "Home", false);
        let chat1 = make_tab(TabContent::empty_chat(), "Chat 1", true);
        let chat2 = make_tab(TabContent::empty_chat(), "Chat 2", true);
        let chat3 = make_tab(TabContent::empty_chat(), "Chat 3", true);
        let target_id = chat1.id;
        let mut cz = CenterZoneState {
            layout: CenterLayout::Single(PaneState {
                tabs: vec![home, chat1, chat2, chat3],
                active_tab_id: Some(target_id),
            }),
        };
        cz.close_to_right(target_id);
        assert_eq!(cz.tabs().len(), 2);
        assert!(cz.tabs().iter().any(|t| !t.closeable));
        assert!(cz.tabs().iter().any(|t| t.id == target_id));
        assert_eq!(cz.active_tab_id(), Some(target_id));
    }

    #[test]
    fn close_all_keeps_only_non_closeable() {
        let home = make_tab(TabContent::Home, "Home", false);
        let home_id = home.id;
        let chat1 = make_tab(TabContent::empty_chat(), "Chat 1", true);
        let chat2 = make_tab(TabContent::empty_chat(), "Chat 2", true);
        let mut cz = CenterZoneState {
            layout: CenterLayout::Single(PaneState {
                tabs: vec![home, chat1, chat2],
                active_tab_id: None,
            }),
        };
        cz.close_all();
        assert_eq!(cz.tabs().len(), 1);
        assert!(matches!(cz.tabs()[0].content, TabContent::Home));
        assert_eq!(cz.active_tab_id(), Some(home_id));
    }

    #[test]
    fn bump_tab_lru_pushes_to_front_dedup_truncate() {
        let state = AppState::new();
        let a = next_tab_id();
        let b = next_tab_id();
        let c = next_tab_id();

        // Wipe the seed (initial home tab) so the test is deterministic.
        state.tab_lru.set(Vec::new());

        state.bump_tab_lru(a);
        state.bump_tab_lru(b);
        // Capacity is 2 — bumping `c` evicts `a`.
        state.bump_tab_lru(c);
        assert_eq!(state.tab_lru.get_untracked(), vec![c, b]);

        // Re-bumping the back-of-LRU tab brings it forward without
        // changing list length.
        state.bump_tab_lru(b);
        assert_eq!(state.tab_lru.get_untracked(), vec![b, c]);
    }

    #[test]
    fn forget_tab_lru_drops_only_target() {
        let state = AppState::new();
        state.tab_lru.set(Vec::new());
        let a = next_tab_id();
        let b = next_tab_id();
        state.bump_tab_lru(a);
        state.bump_tab_lru(b);
        state.forget_tab_lru(a);
        assert_eq!(state.tab_lru.get_untracked(), vec![b]);
    }

    #[test]
    fn prune_tab_lru_drops_ids_not_in_center_zone() {
        let state = AppState::new();
        let live_id = state
            .center_zone
            .with_untracked(|cz| cz.active_tab_id())
            .expect("default home tab is active");
        let stale = next_tab_id();
        // Manually insert a stale id alongside the live one.
        state.tab_lru.set(vec![live_id, stale]);
        state.prune_tab_lru();
        assert_eq!(state.tab_lru.get_untracked(), vec![live_id]);
    }

    #[test]
    fn rename_tab_label_only_changes_target() {
        let home = make_tab(TabContent::Home, "Home", false);
        let chat = make_tab(TabContent::empty_chat(), "Old Name", true);
        let target_id = chat.id;
        let mut cz = CenterZoneState {
            layout: CenterLayout::Single(PaneState {
                tabs: vec![home, chat],
                active_tab_id: None,
            }),
        };
        cz.rename_tab_label(target_id, "New Name".to_string());
        assert_eq!(cz.tabs()[0].label, "Home");
        assert_eq!(cz.tabs()[1].label, "New Name");
    }

    // ── Diff reducer / request-state tests ──────────────────────────────

    fn mk_state(mode: DiffContextMode, pending: bool, files: Vec<&str>) -> DiffViewState {
        DiffViewState {
            root: ProjectRootPath("/r".to_string()),
            scope: ProjectDiffScope::Unstaged,
            path: Some("a.rs".to_string()),
            context_mode: mode,
            pending,
            files: files
                .into_iter()
                .map(|p| ProjectGitDiffFile {
                    relative_path: p.to_string(),
                    is_binary: false,
                    hunks: vec![],
                })
                .collect(),
        }
    }

    fn mk_payload(mode: DiffContextMode, files: Vec<&str>) -> ProjectGitDiffPayload {
        ProjectGitDiffPayload {
            root: ProjectRootPath("/r".to_string()),
            scope: ProjectDiffScope::Unstaged,
            path: Some("a.rs".to_string()),
            context_mode: mode,
            files: files
                .into_iter()
                .map(|p| ProjectGitDiffFile {
                    relative_path: p.to_string(),
                    is_binary: false,
                    hunks: vec![],
                })
                .collect(),
        }
    }

    #[test]
    fn reduce_diff_response_matching_mode_clears_pending() {
        let current = mk_state(DiffContextMode::Hunks, true, vec![]);
        let payload = mk_payload(DiffContextMode::Hunks, vec!["a.rs"]);
        let next = reduce_diff_response(Some(&current), payload).expect("should accept");
        assert!(!next.pending);
        assert_eq!(next.files.len(), 1);
        assert_eq!(next.context_mode, DiffContextMode::Hunks);
    }

    #[test]
    fn reduce_diff_response_rejects_stale_mode() {
        let current = mk_state(DiffContextMode::FullFile, true, vec![]);
        let payload = mk_payload(DiffContextMode::Hunks, vec!["a.rs"]);
        assert!(reduce_diff_response(Some(&current), payload).is_none());
    }

    #[test]
    fn reduce_diff_response_ignores_when_no_outstanding_request() {
        let payload = mk_payload(DiffContextMode::Hunks, vec!["a.rs"]);
        assert!(reduce_diff_response(None, payload).is_none());
    }

    #[test]
    fn for_request_preserves_files_when_mode_unchanged() {
        let prev = mk_state(DiffContextMode::Hunks, false, vec!["a.rs", "b.rs"]);
        let next = DiffViewState::for_request(
            Some(&prev),
            prev.root.clone(),
            prev.scope,
            prev.path.clone(),
            DiffContextMode::Hunks,
        );
        assert!(next.pending);
        assert_eq!(next.files.len(), 2, "files kept across a same-mode refresh");
    }

    #[test]
    fn for_request_clears_files_on_mode_change() {
        let prev = mk_state(DiffContextMode::Hunks, false, vec!["a.rs"]);
        let next = DiffViewState::for_request(
            Some(&prev),
            prev.root.clone(),
            prev.scope,
            prev.path.clone(),
            DiffContextMode::FullFile,
        );
        assert!(next.pending);
        assert!(
            next.files.is_empty(),
            "stale files must not render while a different-mode request is in flight"
        );
        assert_eq!(next.context_mode, DiffContextMode::FullFile);
    }

    #[test]
    fn for_request_with_no_previous_starts_empty_pending() {
        let next = DiffViewState::for_request(
            None,
            ProjectRootPath("/r".to_string()),
            ProjectDiffScope::Staged,
            Some("a.rs".to_string()),
            DiffContextMode::Hunks,
        );
        assert!(next.pending);
        assert!(next.files.is_empty());
    }

    // ── AppState-level batch-close tests ─────────────────────────────────

    fn test_path(name: &str) -> ProjectPath {
        ProjectPath {
            root: ProjectRootPath(format!("/root/{name}")),
            relative_path: format!("{name}.txt"),
        }
    }

    fn test_diff_state(
        root: ProjectRootPath,
        scope: ProjectDiffScope,
        path: Option<String>,
    ) -> DiffViewState {
        DiffViewState {
            root: root.clone(),
            scope,
            path,
            context_mode: DiffContextMode::Hunks,
            pending: false,
            files: vec![],
        }
    }

    fn test_file_key(path: ProjectPath) -> FileResourceKey {
        FileResourceKey {
            host_id: "h".to_string(),
            project_id: ProjectId("p".to_string()),
            path,
        }
    }

    #[test]
    fn close_other_tabs_cleans_backing_state() {
        let owner = leptos::reactive::owner::Owner::new();
        owner.with(|| {
            let state = AppState::new();

            // Open a File tab and a Diff tab, keep Chat as the target
            let file_path = test_path("file_a");
            let file_key = test_file_key(file_path.clone());
            state.center_zone.update(|cz| {
                cz.open(
                    TabContent::File {
                        key: file_key.clone(),
                    },
                    "file_a.txt".to_string(),
                    true,
                );
            });
            state.center_zone.update(|cz| {
                cz.open(TabContent::empty_chat(), "Chat".to_string(), true);
            });
            let target_id = state
                .center_zone
                .with_untracked(|cz| cz.active_tab_id().unwrap());
            let diff_root = ProjectRootPath("/root/proj".to_string());
            let diff_scope = ProjectDiffScope::Unstaged;
            let diff_path = "src/lib.rs".to_string();
            state.center_zone.update(|cz| {
                cz.open(
                    TabContent::Diff {
                        host_id: "h".to_string(),
                        project_id: ProjectId("p".to_string()),
                        root: diff_root.clone(),
                        scope: diff_scope,
                        path: diff_path.clone(),
                    },
                    "Diff".to_string(),
                    true,
                );
            });

            state.open_files.update(|m| {
                m.insert(
                    file_key.clone(),
                    OpenFile {
                        path: file_path.clone(),
                        version: ProjectFileVersion(1),
                        contents: None,
                        is_binary: false,
                        missing: false,
                    },
                );
            });
            let diff_key = DiffKey::new(
                "h",
                ProjectId("p".to_string()),
                diff_root.clone(),
                diff_scope,
                diff_path.clone(),
            );
            state.diff_contents.update(|m| {
                m.insert(
                    diff_key.clone(),
                    test_diff_state(diff_root.clone(), diff_scope, Some(diff_path.clone())),
                );
            });

            state.close_other_tabs(target_id);

            assert!(
                !state
                    .open_files
                    .with_untracked(|m| m.contains_key(&file_key))
            );
            assert!(
                !state
                    .diff_contents
                    .with_untracked(|m| m.contains_key(&diff_key))
            );
            state.center_zone.with_untracked(|cz| {
                assert_eq!(cz.tabs().len(), 2);
                assert!(cz.tabs().iter().any(|t| t.id == target_id));
                assert!(cz.tabs().iter().any(|t| !t.closeable));
            });
        });
    }

    /// Opening diffs for two different files in the same (root, scope) must
    /// create two distinct tabs — they are different views and should not
    /// silently overwrite each other (regression: tabs were keyed only on
    /// (root, scope), which collapsed every diff into a single stale tab).
    #[test]
    fn diffs_for_different_paths_open_separate_tabs() {
        let owner = leptos::reactive::owner::Owner::new();
        owner.with(|| {
            let state = AppState::new();
            let root = ProjectRootPath("/root/proj".to_string());
            let scope = ProjectDiffScope::Unstaged;

            let id_a = state
                .center_zone
                .try_update(|cz| {
                    cz.open(
                        TabContent::Diff {
                            host_id: "h".to_string(),
                            project_id: ProjectId("p".to_string()),
                            root: root.clone(),
                            scope,
                            path: "src/a.rs".to_string(),
                        },
                        "Diff: proj/a.rs".to_string(),
                        true,
                    )
                })
                .unwrap();
            let id_b = state
                .center_zone
                .try_update(|cz| {
                    cz.open(
                        TabContent::Diff {
                            host_id: "h".to_string(),
                            project_id: ProjectId("p".to_string()),
                            root: root.clone(),
                            scope,
                            path: "src/b.rs".to_string(),
                        },
                        "Diff: proj/b.rs".to_string(),
                        true,
                    )
                })
                .unwrap();

            assert_ne!(id_a, id_b, "different paths must produce different tab ids");
            state.center_zone.with_untracked(|cz| {
                let labels: Vec<&str> = cz
                    .all_tabs()
                    .filter(|(_, tab)| matches!(&tab.content, TabContent::Diff { .. }))
                    .map(|(_, tab)| tab.label.as_str())
                    .collect();
                assert_eq!(labels, vec!["Diff: proj/a.rs", "Diff: proj/b.rs"]);
            });

            // Re-opening the same path should reuse the existing tab.
            let id_a2 = state
                .center_zone
                .try_update(|cz| {
                    cz.open(
                        TabContent::Diff {
                            host_id: "h".to_string(),
                            project_id: ProjectId("p".to_string()),
                            root: root.clone(),
                            scope,
                            path: "src/a.rs".to_string(),
                        },
                        "Diff: proj/a.rs".to_string(),
                        true,
                    )
                })
                .unwrap();
            assert_eq!(id_a, id_a2);
        });
    }

    #[test]
    fn close_tabs_to_right_cleans_backing_state() {
        let owner = leptos::reactive::owner::Owner::new();
        owner.with(|| {
            let state = AppState::new();

            state.center_zone.update(|cz| {
                cz.open(TabContent::empty_chat(), "Chat".to_string(), true);
            });
            let target_id = state
                .center_zone
                .with_untracked(|cz| cz.active_tab_id().unwrap());
            let file_path = test_path("file_b");
            let file_key = test_file_key(file_path.clone());
            state.center_zone.update(|cz| {
                cz.open(
                    TabContent::File {
                        key: file_key.clone(),
                    },
                    "file_b.txt".to_string(),
                    true,
                );
            });
            state.open_files.update(|m| {
                m.insert(
                    file_key.clone(),
                    OpenFile {
                        path: file_path.clone(),
                        version: ProjectFileVersion(1),
                        contents: None,
                        is_binary: false,
                        missing: false,
                    },
                );
            });

            state.close_tabs_to_right(target_id);

            assert!(
                !state
                    .open_files
                    .with_untracked(|m| m.contains_key(&file_key))
            );
            state.center_zone.with_untracked(|cz| {
                assert!(cz.tabs().iter().any(|t| t.id == target_id));
                assert!(!cz.tabs().iter().any(|t| {
                    matches!(&t.content, TabContent::File { key } if *key == file_key)
                }));
            });
        });
    }

    #[test]
    fn close_other_tabs_invalid_id_is_noop() {
        let owner = leptos::reactive::owner::Owner::new();
        owner.with(|| {
            let state = AppState::new();
            let file_path = test_path("file_c");
            let file_key = test_file_key(file_path.clone());
            state.center_zone.update(|cz| {
                cz.open(
                    TabContent::File {
                        key: file_key.clone(),
                    },
                    "file_c.txt".to_string(),
                    true,
                );
            });
            state.open_files.update(|m| {
                m.insert(
                    file_key.clone(),
                    OpenFile {
                        path: file_path.clone(),
                        version: ProjectFileVersion(1),
                        contents: None,
                        is_binary: false,
                        missing: false,
                    },
                );
            });

            let tab_count_before = state.center_zone.with_untracked(|cz| cz.tabs().len());
            state.close_other_tabs(TabId(999_999));

            assert_eq!(
                state.center_zone.with_untracked(|cz| cz.tabs().len()),
                tab_count_before
            );
            assert!(
                state
                    .open_files
                    .with_untracked(|m| m.contains_key(&file_key))
            );
        });
    }

    #[test]
    fn active_agent_is_derived_from_active_chat_tab() {
        let owner = leptos::reactive::owner::Owner::new();
        owner.with(|| {
            let state = AppState::new();

            let agent_a = ActiveAgentRef {
                host_id: "host".to_owned(),
                agent_id: AgentId("a".to_owned()),
            };
            let agent_b = ActiveAgentRef {
                host_id: "host".to_owned(),
                agent_id: AgentId("b".to_owned()),
            };

            // Memo starts as None (no chat tab yet).
            assert_eq!(state.active_agent.get_untracked(), None);

            state.open_tab(
                TabContent::chat_with_agent(agent_a.clone()),
                "A".to_owned(),
                true,
            );
            assert_eq!(state.active_agent.get_untracked(), Some(agent_a.clone()));

            let a_tab_id = state
                .center_zone
                .with_untracked(|cz| cz.active_tab_id().expect("A tab active"));

            state.open_tab(
                TabContent::chat_with_agent(agent_b.clone()),
                "B".to_owned(),
                true,
            );
            assert_eq!(state.active_agent.get_untracked(), Some(agent_b.clone()));

            // Closing the active B tab should fall back to A — and the Memo
            // must reflect that, not stay stale on B.
            let b_tab_id = state
                .center_zone
                .with_untracked(|cz| cz.active_tab_id().expect("B tab active"));
            state.close_tab(b_tab_id);
            assert_eq!(state.active_agent.get_untracked(), Some(agent_a.clone()));

            // Closing A leaves only the Home tab (re-created by close()),
            // which is not a Chat — so active_agent is None.
            state.close_tab(a_tab_id);
            assert_eq!(state.active_agent.get_untracked(), None);
        });
    }

    #[test]
    fn chat_context_prefers_active_project_over_settings_selected_host() {
        let owner = leptos::reactive::owner::Owner::new();
        owner.with(|| {
            let state = AppState::new();

            state.selected_host_id.set(Some("host-b".to_owned()));
            state.connection_statuses.update(|statuses| {
                statuses.insert("host-a".to_owned(), ConnectionStatus::Connected);
                statuses.insert("host-b".to_owned(), ConnectionStatus::Disconnected);
            });
            state.host_settings_by_host.update(|settings| {
                settings.insert(
                    "host-a".to_owned(),
                    HostSettings {
                        enabled_backends: vec![BackendKind::Claude],
                        default_backend: Some(BackendKind::Claude),
                        enable_mobile_connections: false,
                        mobile_broker_url: None,
                        tyde_debug_mcp_enabled: false,
                        tyde_agent_control_mcp_enabled: true,
                        complexity_tiers_enabled: false,
                        backend_tier_configs: std::collections::HashMap::new(),
                        background_agent_features: Default::default(),
                        supervisor: Default::default(),
                        code_intel: Default::default(),
                        backend_config: std::collections::HashMap::new(),
                        launch_profiles: Vec::new(),
                    },
                );
                settings.insert(
                    "host-b".to_owned(),
                    HostSettings {
                        enabled_backends: vec![BackendKind::Antigravity],
                        default_backend: Some(BackendKind::Antigravity),
                        enable_mobile_connections: false,
                        mobile_broker_url: None,
                        tyde_debug_mcp_enabled: false,
                        tyde_agent_control_mcp_enabled: true,
                        complexity_tiers_enabled: false,
                        backend_tier_configs: std::collections::HashMap::new(),
                        background_agent_features: Default::default(),
                        supervisor: Default::default(),
                        code_intel: Default::default(),
                        backend_config: std::collections::HashMap::new(),
                        launch_profiles: Vec::new(),
                    },
                );
            });
            state.active_project.set(Some(ActiveProjectRef {
                host_id: "host-a".to_owned(),
                project_id: ProjectId("project-a".to_owned()),
            }));

            assert_eq!(
                state.chat_context_host_id_untracked(),
                Some("host-a".to_owned())
            );
            assert_eq!(
                state.chat_context_connection_status(),
                ConnectionStatus::Connected
            );
            assert_eq!(
                state
                    .chat_context_host_settings_untracked()
                    .and_then(|settings| settings.default_backend),
                Some(BackendKind::Claude)
            );
        });
    }

    #[test]
    fn chat_context_prefers_active_agent_over_active_project() {
        let owner = leptos::reactive::owner::Owner::new();
        owner.with(|| {
            let state = AppState::new();

            state.selected_host_id.set(Some("host-b".to_owned()));
            state.active_project.set(Some(ActiveProjectRef {
                host_id: "host-a".to_owned(),
                project_id: ProjectId("project-a".to_owned()),
            }));

            let agent_ref = ActiveAgentRef {
                host_id: "host-c".to_owned(),
                agent_id: AgentId("agent-c".to_owned()),
            };
            state.open_tab(
                TabContent::chat_with_agent(agent_ref),
                "Agent C".to_owned(),
                true,
            );

            assert_eq!(
                state.chat_context_host_id_untracked(),
                Some("host-c".to_owned())
            );
        });
    }

    #[test]
    fn clear_host_runtime_drops_chat_state_for_host_agents() {
        let owner = leptos::reactive::owner::Owner::new();
        owner.with(|| {
            let state = AppState::new();

            let host_a = "host-a";
            let host_b = "host-b";
            let agent_a1 = AgentId("a1".to_owned());
            let agent_a2 = AgentId("a2".to_owned());
            let agent_b1 = AgentId("b1".to_owned());

            let mk_agent = |host: &str, id: &AgentId| AgentInfo {
                host_id: host.to_owned(),
                agent_id: id.clone(),
                name: format!("{}/{}", host, id.0),
                origin: AgentOrigin::User,
                backend_kind: BackendKind::Tycode,
                workspace_roots: Vec::new(),
                project_id: None,
                parent_agent_id: None,
                session_id: None,
                custom_agent_id: None,
                workflow: None,
                created_at_ms: 0,
                instance_stream: StreamPath(format!("/agents/{}", id.0)),
                started: true,
                fatal_error: None,
                activity_summary: Default::default(),
            };

            state.agents.update(|agents| {
                agents.push(mk_agent(host_a, &agent_a1));
                agents.push(mk_agent(host_a, &agent_a2));
                agents.push(mk_agent(host_b, &agent_b1));
            });

            let mk_msg = || ChatMessageEntry {
                message: ChatMessage {
                    message_id: None,
                    timestamp: 0,
                    sender: protocol::MessageSender::User,
                    content: "hi".to_owned(),
                    reasoning: None,
                    tool_calls: Vec::new(),
                    model_info: None,
                    token_usage: None,
                    context_breakdown: None,
                    images: None,
                },
                tool_requests: Vec::new(),
            };

            for id in [&agent_a1, &agent_a2, &agent_b1] {
                state.chat_rows.update(|m| {
                    m.insert(id.clone(), vec![ChatRowHandle::new(mk_msg())]);
                });
                state.task_lists.update(|m| {
                    m.insert(
                        id.clone(),
                        TaskList {
                            title: String::new(),
                            tasks: Vec::new(),
                        },
                    );
                });
                state.transient_events.update(|m| {
                    m.insert(id.clone(), Vec::new());
                });
                state.orchestration.update(|m| {
                    m.insert(id.clone(), Vec::new());
                });
                state.agent_message_queue.update(|m| {
                    m.insert(id.clone(), Vec::new());
                });
                state.agent_turn_active.update(|m| {
                    m.insert(id.clone(), true);
                });
                state.agent_session_settings.update(|m| {
                    m.insert(id.clone(), SessionSettingsValues::default());
                });
            }

            state.clear_host_runtime(host_a);

            // host_a's agents are forgotten across every per-agent map.
            for id in [&agent_a1, &agent_a2] {
                assert!(
                    !state.chat_rows.with_untracked(|m| m.contains_key(id)),
                    "chat_rows still has dropped agent {}",
                    id.0
                );
                assert!(
                    !state.task_lists.with_untracked(|m| m.contains_key(id)),
                    "task_lists still has dropped agent {}",
                    id.0
                );
                assert!(
                    !state
                        .transient_events
                        .with_untracked(|m| m.contains_key(id)),
                    "transient_events still has dropped agent {}",
                    id.0
                );
                assert!(
                    !state.orchestration.with_untracked(|m| m.contains_key(id)),
                    "orchestration still has dropped agent {}",
                    id.0
                );
                assert!(
                    !state
                        .agent_message_queue
                        .with_untracked(|m| m.contains_key(id)),
                    "agent_message_queue still has dropped agent {}",
                    id.0
                );
                assert!(
                    !state
                        .agent_turn_active
                        .with_untracked(|m| m.contains_key(id)),
                    "agent_turn_active still has dropped agent {}",
                    id.0
                );
                assert!(
                    !state
                        .agent_session_settings
                        .with_untracked(|m| m.contains_key(id)),
                    "agent_session_settings still has dropped agent {}",
                    id.0
                );
            }

            // host_b's agent is untouched.
            assert!(
                state
                    .chat_rows
                    .with_untracked(|m| m.contains_key(&agent_b1)),
                "host_b agent's chat_rows must survive"
            );
            assert!(
                state
                    .task_lists
                    .with_untracked(|m| m.contains_key(&agent_b1)),
                "host_b agent's task_lists must survive"
            );
        });
    }

    #[test]
    fn clear_host_runtime_drops_backend_config_schemas_for_host() {
        let owner = leptos::reactive::owner::Owner::new();
        owner.with(|| {
            let state = AppState::new();
            let host_a = "host-a";
            let host_b = "host-b";

            let schema = |backend_kind: BackendKind| protocol::BackendConfigSchema {
                backend_kind,
                persistence_mode: protocol::BackendConfigPersistenceMode::TydeSettingsStore,
                fields: Vec::new(),
            };

            state.backend_config_schemas.update(|schemas| {
                schemas.insert(
                    host_a.to_owned(),
                    HashMap::from([(BackendKind::Claude, schema(BackendKind::Claude))]),
                );
                schemas.insert(
                    host_b.to_owned(),
                    HashMap::from([(BackendKind::Hermes, schema(BackendKind::Hermes))]),
                );
            });

            state.clear_host_runtime(host_a);

            assert!(
                !state
                    .backend_config_schemas
                    .with_untracked(|schemas| schemas.contains_key(host_a)),
                "host_a backend config schemas must be dropped"
            );
            assert!(
                state
                    .backend_config_schemas
                    .with_untracked(|schemas| schemas.contains_key(host_b)),
                "host_b backend config schemas must survive"
            );
        });
    }

    #[test]
    fn clear_host_runtime_drops_only_that_hosts_project_runtime_state() {
        let owner = leptos::reactive::owner::Owner::new();
        owner.with(|| {
            let state = AppState::new();

            let host_a = "host-a";
            let host_b = "host-b";
            let project_a = ProjectId("project-a".to_owned());
            let project_b = ProjectId("project-b".to_owned());
            let review_a = ReviewId("review-a".to_owned());
            let review_b = ReviewId("review-b".to_owned());
            let path_a = test_path("a");
            let path_b = test_path("b");
            let active_a = ActiveProjectRef {
                host_id: host_a.to_owned(),
                project_id: project_a.clone(),
            };
            let active_b = ActiveProjectRef {
                host_id: host_b.to_owned(),
                project_id: project_b.clone(),
            };

            let mk_project = |host: &str, id: &ProjectId| ProjectInfo {
                host_id: host.to_owned(),
                project: Project {
                    id: id.clone(),
                    name: id.0.clone(),
                    sort_order: 0,
                    source: protocol::ProjectSource::Standalone {
                        roots: vec![ProjectRootPath(format!("/repo/{}", id.0))],
                    },
                },
            };
            let mk_review = |id: &ReviewId, project_id: &ProjectId| Review {
                id: id.clone(),
                project_id: project_id.clone(),
                origin_agent_id: AgentId(format!("agent-{}", id.0)),
                origin_session_id: SessionId(format!("session-{}", id.0)),
                selection: protocol::ReviewDiffSelection::Workspace {
                    scope: ProjectDiffScope::Unstaged,
                },
                status: protocol::ReviewStatus::Draft,
                diffs: Vec::new(),
                comments: Vec::new(),
                suggestions: Vec::new(),
                ai_reviewer: protocol::ReviewAiReviewerState {
                    status: protocol::ReviewAiReviewerStatus::Idle,
                    agent_id: None,
                    error: None,
                },
                created_at_ms: 0,
                updated_at_ms: 0,
            };
            let mk_summary = |id: &ReviewId| ReviewSummary {
                id: id.clone(),
                scope: protocol::ReviewSummaryScope::Workspace,
                status: protocol::ReviewStatus::Draft,
                origin_session_id: SessionId(format!("session-{}", id.0)),
                origin_agent_id: AgentId(format!("agent-{}", id.0)),
                created_at_ms: 0,
                updated_at_ms: 0,
                user_comment_count: 1,
                pending_suggestion_count: 0,
                file_comment_counts: Vec::new(),
            };
            let mk_diff_key = |host: &str, project_id: &ProjectId, name: &str| {
                DiffKey::new(
                    host,
                    project_id.clone(),
                    ProjectRootPath(format!("/repo/{name}")),
                    ProjectDiffScope::Unstaged,
                    "",
                )
            };
            let mk_diff_state = |name: &str| DiffViewState {
                root: ProjectRootPath(format!("/repo/{name}")),
                scope: ProjectDiffScope::Unstaged,
                path: None,
                context_mode: DiffContextMode::Hunks,
                pending: false,
                files: Vec::new(),
            };

            state.projects.update(|projects| {
                projects.push(mk_project(host_a, &project_a));
                projects.push(mk_project(host_b, &project_b));
            });
            state.file_tree.update(|map| {
                map.insert(
                    project_a.clone(),
                    vec![ProjectRootListing {
                        root: ProjectRootPath("/repo/a".to_owned()),
                        entries: Vec::new(),
                    }],
                );
                map.insert(
                    project_b.clone(),
                    vec![ProjectRootListing {
                        root: ProjectRootPath("/repo/b".to_owned()),
                        entries: Vec::new(),
                    }],
                );
            });
            state.git_status.update(|map| {
                map.insert(
                    project_a.clone(),
                    vec![ProjectRootGitStatus {
                        root: ProjectRootPath("/repo/a".to_owned()),
                        branch: None,
                        ahead: 0,
                        behind: 0,
                        clean: true,
                        files: Vec::new(),
                    }],
                );
                map.insert(
                    project_b.clone(),
                    vec![ProjectRootGitStatus {
                        root: ProjectRootPath("/repo/b".to_owned()),
                        branch: None,
                        ahead: 0,
                        behind: 0,
                        clean: true,
                        files: Vec::new(),
                    }],
                );
            });
            state.reviews.update(|map| {
                map.insert(review_a.clone(), mk_review(&review_a, &project_a));
                map.insert(review_b.clone(), mk_review(&review_b, &project_b));
            });
            state.review_summaries.update(|map| {
                map.insert(project_a.clone(), vec![mk_summary(&review_a)]);
                map.insert(project_b.clone(), vec![mk_summary(&review_b)]);
            });
            state.review_action_pending.update(|map| {
                map.insert(
                    review_a.clone(),
                    ReviewActionGate {
                        submit: true,
                        ..ReviewActionGate::default()
                    },
                );
                map.insert(
                    review_b.clone(),
                    ReviewActionGate {
                        submit: true,
                        ..ReviewActionGate::default()
                    },
                );
            });
            state.review_action_target_pending.update(|set| {
                set.insert((review_a.clone(), ReviewActionTarget::AddComment));
                set.insert((review_b.clone(), ReviewActionTarget::AddComment));
            });
            state.review_create_pending.update(|map| {
                map.insert((host_a.to_owned(), project_a.clone()), 1);
                map.insert((host_b.to_owned(), project_b.clone()), 1);
            });
            state.code_intel.update(|map| {
                map.insert(
                    CodeIntelKey {
                        host_id: host_a.to_owned(),
                        project_id: project_a.clone(),
                        path: path_a.clone(),
                    },
                    CodeIntelFileState::default(),
                );
                map.insert(
                    CodeIntelKey {
                        host_id: host_b.to_owned(),
                        project_id: project_b.clone(),
                        path: path_b.clone(),
                    },
                    CodeIntelFileState::default(),
                );
            });
            let stray_diff_key_a = mk_diff_key(host_a, &project_a, "stray-a");
            let stray_diff_state_a = mk_diff_state("stray-a");
            let diff_key_b = mk_diff_key(host_b, &project_b, "b");
            let diff_state_b = mk_diff_state("b");
            state.diff_contents.update(|map| {
                map.insert(mk_diff_key(host_a, &project_a, "a"), mk_diff_state("a"));
                map.insert(diff_key_b.clone(), diff_state_b.clone());
            });
            state
                .code_intel_navigate_ctx
                .set(Some(CodeIntelNavigateContext {
                    navigate_id: 1,
                    tab: next_tab_id(),
                    key: FileResourceKey {
                        host_id: host_a.to_owned(),
                        project_id: project_a.clone(),
                        path: path_a.clone(),
                    },
                    version: ProjectFileVersion(1),
                }));
            state.project_view_memory.update(|map| {
                map.insert(active_a.clone(), ProjectViewMemory::default());
                map.insert(
                    active_b.clone(),
                    ProjectViewMemory {
                        diff_contents: HashMap::from([
                            (stray_diff_key_a.clone(), stray_diff_state_a.clone()),
                            (diff_key_b.clone(), diff_state_b.clone()),
                        ]),
                        ..ProjectViewMemory::default()
                    },
                );
            });
            state.sessions_panel_filters.update(|map| {
                map.insert(Some(active_a.clone()), SessionsPanelFilters::default());
                map.insert(Some(active_b.clone()), SessionsPanelFilters::default());
                map.insert(None, SessionsPanelFilters::default());
            });
            state.switch_active_project(Some(active_a.clone()));

            state.clear_host_runtime(host_a);

            assert_eq!(state.active_project.get_untracked(), None);
            assert!(
                !state.projects.with_untracked(|projects| {
                    projects
                        .iter()
                        .any(|project| project.host_id == host_a || project.project.id == project_a)
                }),
                "host_a project metadata must be removed"
            );
            assert!(
                state.projects.with_untracked(|projects| {
                    projects
                        .iter()
                        .any(|project| project.host_id == host_b && project.project.id == project_b)
                }),
                "host_b project metadata must survive"
            );
            assert!(
                !state
                    .file_tree
                    .with_untracked(|m| m.contains_key(&project_a))
            );
            assert!(
                state
                    .file_tree
                    .with_untracked(|m| m.contains_key(&project_b))
            );
            assert!(
                !state
                    .git_status
                    .with_untracked(|m| m.contains_key(&project_a))
            );
            assert!(
                state
                    .git_status
                    .with_untracked(|m| m.contains_key(&project_b))
            );
            assert!(!state.reviews.with_untracked(|m| m.contains_key(&review_a)));
            assert!(state.reviews.with_untracked(|m| m.contains_key(&review_b)));
            assert!(
                !state
                    .review_summaries
                    .with_untracked(|m| m.contains_key(&project_a))
            );
            assert!(
                state
                    .review_summaries
                    .with_untracked(|m| m.contains_key(&project_b))
            );
            assert!(
                !state
                    .review_action_pending
                    .with_untracked(|m| m.contains_key(&review_a))
            );
            assert!(
                state
                    .review_action_pending
                    .with_untracked(|m| m.contains_key(&review_b))
            );
            assert!(
                !state
                    .review_action_target_pending
                    .with_untracked(|set| set.iter().any(|(review_id, _)| review_id == &review_a))
            );
            assert!(
                state
                    .review_action_target_pending
                    .with_untracked(|set| set.iter().any(|(review_id, _)| review_id == &review_b))
            );
            assert!(
                !state
                    .review_create_pending
                    .with_untracked(|m| m.contains_key(&(host_a.to_owned(), project_a.clone())))
            );
            assert!(
                state
                    .review_create_pending
                    .with_untracked(|m| m.contains_key(&(host_b.to_owned(), project_b.clone())))
            );
            assert!(
                !state
                    .code_intel
                    .with_untracked(|m| m.keys().any(|key| key.host_id == host_a))
            );
            assert!(
                state
                    .code_intel
                    .with_untracked(|m| m.keys().any(|key| key.host_id == host_b))
            );
            assert!(
                !state
                    .diff_contents
                    .with_untracked(|m| m.keys().any(|key| key.host_id == host_a))
            );
            assert_eq!(state.code_intel_navigate_ctx.get_untracked(), None);
            assert!(
                !state
                    .project_view_memory
                    .with_untracked(|m| m.keys().any(|key| key.host_id == host_a))
            );
            assert!(
                state
                    .project_view_memory
                    .with_untracked(|m| m.keys().any(|key| key.host_id == host_b))
            );
            assert!(state.project_view_memory.with_untracked(|m| {
                m.get(&active_b).is_some_and(|memory| {
                    memory.diff_contents.keys().all(|key| key.host_id != host_a)
                        && memory.diff_contents.keys().any(|key| key.host_id == host_b)
                })
            }));
            assert!(!state.sessions_panel_filters.with_untracked(|m| {
                m.keys()
                    .any(|key| key.as_ref().is_some_and(|key| key.host_id == host_a))
            }));
            assert!(state.sessions_panel_filters.with_untracked(|m| {
                m.contains_key(&Some(active_b.clone())) && m.contains_key(&None)
            }));
            // Agents-view preferences (the former `agents_panel_filters` /
            // `agent_monitor_order` local signals) are deliberately no longer
            // pruned on host cleanup. They are server-owned durable state — the
            // old per-host pruning was the flicker source this work removes — so
            // there is nothing host-scoped left here to assert.
        });
    }

    #[test]
    fn forgetting_project_memory_cleans_exact_occurrences_and_keeps_survivors() {
        let owner = leptos::reactive::owner::Owner::new();
        owner.with(|| {
            let state = AppState::new();
            let current_survivor = state
                .center_zone
                .with_untracked(|center_zone| center_zone.active_tab_id())
                .expect("current home tab");

            let mut removed_center = CenterZoneState::default();
            let removed_home = removed_center.active_tab_id().expect("remembered home tab");
            let removed_chat =
                removed_center.open(TabContent::empty_chat(), "Remembered chat".to_owned(), true);
            let removed_project = ActiveProjectRef {
                host_id: "host".to_owned(),
                project_id: ProjectId("removed".to_owned()),
            };

            let survivor_center = CenterZoneState::default();
            let remembered_survivor = survivor_center
                .active_tab_id()
                .expect("surviving remembered home tab");
            let survivor_project = ActiveProjectRef {
                host_id: "host".to_owned(),
                project_id: ProjectId("survivor".to_owned()),
            };
            state.project_view_memory.update(|memories| {
                memories.insert(
                    removed_project.clone(),
                    ProjectViewMemory {
                        center_zone: Some(removed_center),
                        ..ProjectViewMemory::default()
                    },
                );
                memories.insert(
                    survivor_project.clone(),
                    ProjectViewMemory {
                        center_zone: Some(survivor_center),
                        ..ProjectViewMemory::default()
                    },
                );
            });

            state.tab_lru.set(vec![
                removed_home,
                removed_chat,
                current_survivor,
                remembered_survivor,
            ]);
            for (tab, scroll_top) in [
                (removed_home, 10),
                (removed_chat, 20),
                (current_survivor, 30),
                (remembered_survivor, 40),
            ] {
                state.save_tab_scroll_state(
                    tab,
                    TabScrollState {
                        scroll_top,
                        scroll_height: 100,
                        client_height: 20,
                        user_scrolled_up: true,
                    },
                );
            }

            state.forget_project_view_memory(&removed_project);

            assert!(
                !state
                    .project_view_memory
                    .with_untracked(|memories| memories.contains_key(&removed_project))
            );
            assert!(
                state
                    .project_view_memory
                    .with_untracked(|memories| memories.contains_key(&survivor_project))
            );
            assert_eq!(
                state.tab_lru.get_untracked(),
                vec![current_survivor, remembered_survivor]
            );
            assert_eq!(state.tab_scroll_state_untracked(removed_home), None);
            assert_eq!(state.tab_scroll_state_untracked(removed_chat), None);
            assert_eq!(
                state
                    .tab_scroll_state_untracked(current_survivor)
                    .map(|scroll| scroll.scroll_top),
                Some(30)
            );
            assert_eq!(
                state
                    .tab_scroll_state_untracked(remembered_survivor)
                    .map(|scroll| scroll.scroll_top),
                Some(40)
            );
        });
    }

    #[test]
    fn compaction_cleanup_forgets_removed_occurrences_in_current_and_memory() {
        let owner = leptos::reactive::owner::Owner::new();
        owner.with(|| {
            let state = AppState::new();
            let host_id = "host-a";
            let agent_id = AgentId("compacted-agent".to_owned());
            let agent_ref = ActiveAgentRef {
                host_id: host_id.to_owned(),
                agent_id: agent_id.clone(),
            };
            let current_survivor = state
                .center_zone
                .with_untracked(|center_zone| center_zone.active_tab_id())
                .expect("home tab");
            let current_removed = state
                .center_zone
                .try_update(|center_zone| {
                    center_zone.open(
                        TabContent::chat_with_agent(agent_ref.clone()),
                        "Current agent".to_owned(),
                        true,
                    )
                })
                .expect("current chat opens");

            let mut remembered = CenterZoneState::default();
            let memory_survivor = remembered.active_tab_id().expect("remembered home tab");
            let memory_removed = remembered.open(
                TabContent::chat_with_agent(agent_ref),
                "Remembered agent".to_owned(),
                true,
            );
            let remembered_project = ActiveProjectRef {
                host_id: host_id.to_owned(),
                project_id: ProjectId("remembered".to_owned()),
            };
            state.project_view_memory.update(|memories| {
                memories.insert(
                    remembered_project.clone(),
                    ProjectViewMemory {
                        center_zone: Some(remembered),
                        ..ProjectViewMemory::default()
                    },
                );
            });

            state.tab_lru.set(vec![
                current_removed,
                memory_removed,
                current_survivor,
                memory_survivor,
            ]);
            for (tab, scroll_top) in [
                (current_removed, 10),
                (memory_removed, 20),
                (current_survivor, 30),
                (memory_survivor, 40),
            ] {
                state.save_tab_scroll_state(
                    tab,
                    TabScrollState {
                        scroll_top,
                        scroll_height: 100,
                        client_height: 20,
                        user_scrolled_up: true,
                    },
                );
            }

            state.finalize_compaction_close(host_id, &agent_id);

            assert!(
                state
                    .center_zone
                    .with_untracked(|center_zone| center_zone.tab(current_removed).is_none())
            );
            assert!(state.project_view_memory.with_untracked(|memories| {
                memories
                    .get(&remembered_project)
                    .and_then(|memory| memory.center_zone.as_ref())
                    .is_some_and(|center_zone| center_zone.tab(memory_removed).is_none())
            }));
            assert!(state.tab_lru.with_untracked(|lru| {
                !lru.contains(&current_removed) && !lru.contains(&memory_removed)
            }));
            assert_eq!(state.tab_scroll_state_untracked(current_removed), None);
            assert_eq!(state.tab_scroll_state_untracked(memory_removed), None);
            assert_eq!(
                state
                    .tab_scroll_state_untracked(current_survivor)
                    .map(|scroll| scroll.scroll_top),
                Some(30)
            );
            assert_eq!(
                state
                    .tab_scroll_state_untracked(memory_survivor)
                    .map(|scroll| scroll.scroll_top),
                Some(40)
            );
        });
    }

    #[test]
    fn host_cleanup_forgets_removed_occurrences_in_current_and_memory() {
        let owner = leptos::reactive::owner::Owner::new();
        owner.with(|| {
            let state = AppState::new();
            let host_a = "host-a";
            let host_b = "host-b";
            let project_a = ProjectId("project-a".to_owned());
            let project_b = ProjectId("project-b".to_owned());
            let root = ProjectRootPath("/repo".to_owned());
            let current_survivor = state
                .center_zone
                .with_untracked(|center_zone| center_zone.active_tab_id())
                .expect("home tab");
            let current_removed = state
                .center_zone
                .try_update(|center_zone| {
                    center_zone.open(
                        TabContent::Diff {
                            host_id: host_a.to_owned(),
                            project_id: project_a.clone(),
                            root: root.clone(),
                            scope: ProjectDiffScope::Unstaged,
                            path: "src/current.rs".to_owned(),
                        },
                        "Current diff".to_owned(),
                        true,
                    )
                })
                .expect("current diff opens");

            let mut remembered = CenterZoneState::default();
            let memory_survivor = remembered.open(
                TabContent::Diff {
                    host_id: host_b.to_owned(),
                    project_id: project_b.clone(),
                    root: root.clone(),
                    scope: ProjectDiffScope::Unstaged,
                    path: "src/survivor.rs".to_owned(),
                },
                "Surviving diff".to_owned(),
                true,
            );
            let memory_removed = remembered.open(
                TabContent::Diff {
                    host_id: host_a.to_owned(),
                    project_id: project_a.clone(),
                    root: root.clone(),
                    scope: ProjectDiffScope::Staged,
                    path: "src/remembered.rs".to_owned(),
                },
                "Remembered diff".to_owned(),
                true,
            );
            let remembered_project = ActiveProjectRef {
                host_id: host_b.to_owned(),
                project_id: project_b.clone(),
            };
            let mut discarded_memory = CenterZoneState::default();
            let discarded_file = discarded_memory.open(
                TabContent::File {
                    key: resource_key(host_a, &project_a.0, test_path("discarded-host-file")),
                },
                "Discarded file".to_owned(),
                true,
            );
            let discarded_project = ActiveProjectRef {
                host_id: host_a.to_owned(),
                project_id: project_a,
            };
            state.project_view_memory.update(|memories| {
                memories.insert(
                    remembered_project.clone(),
                    ProjectViewMemory {
                        center_zone: Some(remembered),
                        ..ProjectViewMemory::default()
                    },
                );
                memories.insert(
                    discarded_project.clone(),
                    ProjectViewMemory {
                        center_zone: Some(discarded_memory),
                        ..ProjectViewMemory::default()
                    },
                );
            });

            state.tab_lru.set(vec![
                current_removed,
                memory_removed,
                current_survivor,
                memory_survivor,
                discarded_file,
            ]);
            for (tab, scroll_top) in [
                (current_removed, 11),
                (memory_removed, 22),
                (current_survivor, 33),
                (memory_survivor, 44),
                (discarded_file, 55),
            ] {
                state.save_tab_scroll_state(
                    tab,
                    TabScrollState {
                        scroll_top,
                        scroll_height: 100,
                        client_height: 20,
                        user_scrolled_up: true,
                    },
                );
            }

            state.clear_host_runtime(host_a);

            assert!(
                state
                    .center_zone
                    .with_untracked(|center_zone| center_zone.tab(current_removed).is_none())
            );
            assert!(state.project_view_memory.with_untracked(|memories| {
                memories
                    .get(&remembered_project)
                    .and_then(|memory| memory.center_zone.as_ref())
                    .is_some_and(|center_zone| {
                        center_zone.tab(memory_removed).is_none()
                            && center_zone.tab(memory_survivor).is_some()
                    })
            }));
            assert!(
                !state
                    .project_view_memory
                    .with_untracked(|memories| memories.contains_key(&discarded_project))
            );
            assert!(state.tab_lru.with_untracked(|lru| {
                !lru.contains(&current_removed)
                    && !lru.contains(&memory_removed)
                    && !lru.contains(&discarded_file)
            }));
            assert_eq!(state.tab_scroll_state_untracked(current_removed), None);
            assert_eq!(state.tab_scroll_state_untracked(memory_removed), None);
            assert_eq!(state.tab_scroll_state_untracked(discarded_file), None);
            assert_eq!(
                state
                    .tab_scroll_state_untracked(current_survivor)
                    .map(|scroll| scroll.scroll_top),
                Some(33)
            );
            assert_eq!(
                state
                    .tab_scroll_state_untracked(memory_survivor)
                    .map(|scroll| scroll.scroll_top),
                Some(44)
            );
        });
    }

    #[test]
    fn clear_host_runtime_closes_host_tabs_even_without_agent_record() {
        let owner = leptos::reactive::owner::Owner::new();
        owner.with(|| {
            let state = AppState::new();
            let host_a = "host-a";
            let host_b = "host-b";
            let agent_a = ActiveAgentRef {
                host_id: host_a.to_owned(),
                agent_id: AgentId("missing-agent-a".to_owned()),
            };
            let agent_b = ActiveAgentRef {
                host_id: host_b.to_owned(),
                agent_id: AgentId("agent-b".to_owned()),
            };
            let project_a = ProjectId("project-a".to_owned());
            let project_b = ProjectId("project-b".to_owned());
            let root_a = ProjectRootPath("/repo/a".to_owned());
            let root_b = ProjectRootPath("/repo/b".to_owned());

            let mut host_a_tab_ids = Vec::new();
            let mut host_b_tab_id = None;
            state.center_zone.update(|cz| {
                host_a_tab_ids.push(cz.open(
                    TabContent::chat_with_agent(agent_a.clone()),
                    "stale agent".to_owned(),
                    true,
                ));
                host_a_tab_ids.push(cz.open(
                    TabContent::team_member_draft(
                        host_a.to_owned(),
                        TeamMemberId("member-a".to_owned()),
                    ),
                    "team draft".to_owned(),
                    true,
                ));
                host_a_tab_ids.push(cz.open(
                    TabContent::Diff {
                        host_id: host_a.to_owned(),
                        project_id: project_a.clone(),
                        root: root_a.clone(),
                        scope: ProjectDiffScope::Unstaged,
                        path: "src/lib.rs".to_owned(),
                    },
                    "diff".to_owned(),
                    true,
                ));
                host_a_tab_ids.push(cz.open(
                    TabContent::Comments {
                        host_id: host_a.to_owned(),
                        project_id: project_a.clone(),
                    },
                    "comments".to_owned(),
                    true,
                ));
                host_a_tab_ids.push(cz.open(
                    TabContent::Workflow {
                        agent_ref: agent_a.clone(),
                        tool_call_id: ToolCallId("tool-a".to_owned()),
                    },
                    "workflow".to_owned(),
                    true,
                ));
                host_b_tab_id = Some(cz.open(
                    TabContent::Diff {
                        host_id: host_b.to_owned(),
                        project_id: project_b.clone(),
                        root: root_b,
                        scope: ProjectDiffScope::Unstaged,
                        path: "src/main.rs".to_owned(),
                    },
                    "host b diff".to_owned(),
                    true,
                ));
                cz.open(
                    TabContent::chat_with_agent(agent_b),
                    "host b agent".to_owned(),
                    true,
                );
            });
            state.tab_lru.set(host_a_tab_ids.clone());

            state.clear_host_runtime(host_a);

            assert!(state.center_zone.with_untracked(|cz| {
                cz.tabs().iter().all(|tab| match &tab.content {
                    TabContent::Chat {
                        agent_ref,
                        pending_team_member,
                    } => {
                        agent_ref
                            .as_ref()
                            .is_none_or(|agent_ref| agent_ref.host_id != host_a)
                            && pending_team_member
                                .as_ref()
                                .is_none_or(|pending| pending.host_id != host_a)
                    }
                    TabContent::Diff { host_id, .. } | TabContent::Comments { host_id, .. } => {
                        host_id != host_a
                    }
                    TabContent::Workflow { agent_ref, .. } => agent_ref.host_id != host_a,
                    TabContent::Home | TabContent::AgentMonitor | TabContent::File { .. } => true,
                })
            }));
            assert!(state.center_zone.with_untracked(|cz| {
                cz.tabs().iter().any(|tab| Some(tab.id) == host_b_tab_id)
            }));
            assert!(state.tab_lru.with_untracked(|lru| {
                host_a_tab_ids.iter().all(|tab_id| !lru.contains(tab_id))
            }));
        });
    }

    #[test]
    fn close_tabs_to_right_invalid_id_is_noop() {
        let owner = leptos::reactive::owner::Owner::new();
        owner.with(|| {
            let state = AppState::new();
            let file_path = test_path("file_d");
            let file_key = test_file_key(file_path.clone());
            state.center_zone.update(|cz| {
                cz.open(
                    TabContent::File {
                        key: file_key.clone(),
                    },
                    "file_d.txt".to_string(),
                    true,
                );
            });
            state.open_files.update(|m| {
                m.insert(
                    file_key.clone(),
                    OpenFile {
                        path: file_path.clone(),
                        version: ProjectFileVersion(1),
                        contents: None,
                        is_binary: false,
                        missing: false,
                    },
                );
            });

            let tab_count_before = state.center_zone.with_untracked(|cz| cz.tabs().len());
            state.close_tabs_to_right(TabId(999_998));

            assert_eq!(
                state.center_zone.with_untracked(|cz| cz.tabs().len()),
                tab_count_before
            );
            assert!(
                state
                    .open_files
                    .with_untracked(|m| m.contains_key(&file_key))
            );
        });
    }

    fn resource_key(host: &str, project: &str, path: ProjectPath) -> FileResourceKey {
        FileResourceKey {
            host_id: host.to_string(),
            project_id: ProjectId(project.to_string()),
            path,
        }
    }

    #[derive(Debug, PartialEq)]
    struct CenterSnapshot {
        focused: PaneId,
        ratio: Option<SplitRatio>,
        panes: Vec<PaneSnapshot>,
    }

    #[derive(Debug, PartialEq)]
    struct PaneSnapshot {
        pane: PaneId,
        active: Option<TabId>,
        tabs: Vec<(TabId, TabContent, String, bool)>,
    }

    fn center_snapshot(center_zone: &CenterZoneState) -> CenterSnapshot {
        CenterSnapshot {
            focused: center_zone.focused_id(),
            ratio: center_zone.split_ratio(),
            panes: center_zone
                .panes()
                .map(|(pane, state)| PaneSnapshot {
                    pane,
                    active: state.active_tab_id,
                    tabs: state
                        .tabs
                        .iter()
                        .map(|tab| {
                            (
                                tab.id,
                                tab.content.clone(),
                                tab.label.clone(),
                                tab.closeable,
                            )
                        })
                        .collect(),
                })
                .collect(),
        }
    }

    fn install_loaded_file(state: &AppState, pane: PaneId, key: FileResourceKey) -> TabId {
        state.open_files.update(|files| {
            files.insert(
                key.clone(),
                OpenFile {
                    path: key.path.clone(),
                    version: ProjectFileVersion(1),
                    contents: Some("one\ntwo\nthree".to_string()),
                    is_binary: false,
                    missing: false,
                },
            );
        });
        state
            .open_tab_in(pane, TabContent::File { key }, "main.rs".to_string(), true)
            .expect("loaded file opens")
    }

    #[test]
    fn split_ratio_clamps_every_constructor_input() {
        assert_eq!(SplitRatio::new(-1.0).get(), SplitRatio::MIN);
        assert_eq!(SplitRatio::new(2.0).get(), SplitRatio::MAX);
        assert_eq!(SplitRatio::new(f64::NAN).get(), SplitRatio::DEFAULT);
        assert_eq!(SplitRatio::new(f64::INFINITY).get(), SplitRatio::DEFAULT);
    }

    #[test]
    fn duplicate_file_eligibility_distinguishes_refusals_without_mutation() {
        let owner = leptos::reactive::owner::Owner::new();
        owner.with(|| {
            let state = AppState::new();
            let home = state
                .center_zone
                .with_untracked(|center_zone| center_zone.active_tab_id())
                .expect("home tab");
            let before_home = state.center_zone.with_untracked(center_snapshot);
            let not_a_file = state.duplicate_file_eligibility_at(OpenTarget::Beside, home);
            assert_eq!(not_a_file, DuplicateFileEligibility::NotAFile);
            assert_eq!(
                not_a_file.disabled_reason(),
                Some(DUPLICATE_FILE_NOT_A_FILE_REASON)
            );
            assert_eq!(
                state.duplicate_file_at_result(OpenTarget::Beside, home),
                DuplicateFileResult::NotAFile
            );
            assert_eq!(
                state.center_zone.with_untracked(center_snapshot),
                before_home
            );

            state.tabs_enabled.set(false);
            let tabs_disabled = state.duplicate_file_eligibility_at(OpenTarget::Beside, home);
            assert_eq!(tabs_disabled, DuplicateFileEligibility::TabsDisabled);
            assert_eq!(
                tabs_disabled.disabled_reason(),
                Some(DUPLICATE_FILE_TABS_DISABLED_REASON)
            );
            assert_eq!(
                state.duplicate_file_at_result(OpenTarget::Beside, home),
                DuplicateFileResult::TabsDisabled
            );
            assert_eq!(
                state.center_zone.with_untracked(center_snapshot),
                before_home
            );

            state.tabs_enabled.set(true);
            let missing = state.duplicate_file_eligibility_in(PaneId::Secondary, TabId(u64::MAX));
            assert_eq!(missing, DuplicateFileEligibility::SourceTabMissing);
            assert_eq!(
                missing.disabled_reason(),
                Some(DUPLICATE_FILE_SOURCE_MISSING_REASON)
            );
            assert_eq!(
                state.duplicate_file_in_result(PaneId::Secondary, TabId(u64::MAX)),
                DuplicateFileResult::SourceTabMissing
            );
            assert_eq!(
                state.center_zone.with_untracked(center_snapshot),
                before_home
            );

            let unloaded = state
                .open_tab_in(
                    PaneId::Primary,
                    TabContent::File {
                        key: resource_key("h", "p", test_path("typed-unloaded")),
                    },
                    "loading.rs".to_owned(),
                    true,
                )
                .expect("unloaded file tab");
            let before_unloaded = state.center_zone.with_untracked(center_snapshot);
            let not_loaded = state.duplicate_file_eligibility_in(PaneId::Secondary, unloaded);
            assert_eq!(not_loaded, DuplicateFileEligibility::NotLoaded);
            assert_eq!(
                not_loaded.disabled_reason(),
                Some(DUPLICATE_FILE_NOT_LOADED_REASON)
            );
            let refused = state.duplicate_file_in_result(PaneId::Secondary, unloaded);
            assert_eq!(refused, DuplicateFileResult::NotLoaded);
            assert_eq!(
                refused.disabled_reason(),
                Some(DUPLICATE_FILE_NOT_LOADED_REASON)
            );
            assert_eq!(
                state.center_zone.with_untracked(center_snapshot),
                before_unloaded
            );
        });
    }

    #[test]
    fn duplicate_file_result_duplicates_then_activates_existing_occurrence() {
        let owner = leptos::reactive::owner::Owner::new();
        owner.with(|| {
            let state = AppState::new();
            let key = resource_key("h", "p", test_path("typed-duplicate"));
            let primary = install_loaded_file(&state, PaneId::Primary, key.clone());
            let before_query = state.center_zone.with_untracked(center_snapshot);
            assert_eq!(
                state.duplicate_file_eligibility_in(PaneId::Secondary, primary),
                DuplicateFileEligibility::Enabled
            );
            assert_eq!(
                state.center_zone.with_untracked(center_snapshot),
                before_query
            );

            let duplicated = state.duplicate_file_in_result(PaneId::Secondary, primary);
            let DuplicateFileResult::Duplicated {
                source,
                tab: secondary,
                target,
            } = duplicated
            else {
                panic!("loaded file should duplicate: {duplicated:?}");
            };
            assert_eq!(source, primary);
            assert_eq!(target, PaneId::Secondary);
            assert_ne!(secondary, primary);
            assert_eq!(duplicated.disabled_reason(), None);
            assert_eq!(
                state.center_zone.with_untracked(|center_zone| {
                    center_zone.occurrences(&TabContent::File { key: key.clone() })
                }),
                vec![(PaneId::Primary, primary), (PaneId::Secondary, secondary)]
            );

            assert!(state.reveal_tab(primary));
            let before_existing_query = state.center_zone.with_untracked(center_snapshot);
            let already_present = state.duplicate_file_eligibility_in(PaneId::Secondary, primary);
            assert_eq!(
                already_present,
                DuplicateFileEligibility::TargetAlreadyContainsResource {
                    existing: secondary
                }
            );
            assert!(already_present.is_enabled());
            assert_eq!(already_present.disabled_reason(), None);
            assert_eq!(
                state.center_zone.with_untracked(center_snapshot),
                before_existing_query
            );

            assert_eq!(
                state.duplicate_file_in_result(PaneId::Secondary, primary),
                DuplicateFileResult::ActivatedExisting {
                    source: primary,
                    existing: secondary,
                    target: PaneId::Secondary,
                }
            );
            state.center_zone.with_untracked(|center_zone| {
                assert_eq!(center_zone.focused_id(), PaneId::Secondary);
                assert_eq!(
                    center_zone.pane_active_tab_id(PaneId::Secondary),
                    Some(secondary)
                );
                assert_eq!(
                    center_zone.occurrences(&TabContent::File { key }),
                    vec![(PaneId::Primary, primary), (PaneId::Secondary, secondary)]
                );
            });
        });
    }

    #[test]
    fn file_may_have_one_occurrence_per_pane() {
        let owner = leptos::reactive::owner::Owner::new();
        owner.with(|| {
            let state = AppState::new();
            let key = resource_key("h", "p", test_path("unique"));
            let primary = install_loaded_file(&state, PaneId::Primary, key.clone());
            let secondary = state
                .duplicate_file_at_result(OpenTarget::Beside, primary)
                .tab_id()
                .expect("loaded file duplicates beside");

            assert_ne!(primary, secondary);
            assert_eq!(
                state.center_zone.with_untracked(|center_zone| {
                    center_zone
                        .occurrences(&TabContent::File { key: key.clone() })
                        .len()
                }),
                2
            );
            state.activate_tab(primary);
            assert_eq!(
                state.duplicate_file_at_result(OpenTarget::Beside, primary),
                DuplicateFileResult::ActivatedExisting {
                    source: primary,
                    existing: secondary,
                    target: PaneId::Secondary,
                },
                "a second duplicate in the same pane activates the existing occurrence"
            );
            assert_eq!(state.open_files.with_untracked(|files| files.len()), 1);
            assert!(
                state
                    .pending_file_opens
                    .with_untracked(|pending| pending.is_empty())
            );
        });
    }

    #[test]
    fn unloaded_file_cannot_be_duplicated() {
        let owner = leptos::reactive::owner::Owner::new();
        owner.with(|| {
            let state = AppState::new();
            let key = resource_key("h", "p", test_path("loading"));
            let tab = state
                .open_tab_in(
                    PaneId::Primary,
                    TabContent::File { key },
                    "loading.txt".to_string(),
                    true,
                )
                .expect("first occurrence opens");
            assert_eq!(
                state.duplicate_file_at_result(OpenTarget::Beside, tab),
                DuplicateFileResult::NotLoaded
            );
            assert!(!state.center_zone.with_untracked(CenterZoneState::is_split));
        });
    }

    #[test]
    fn chats_are_nonduplicable_across_panes() {
        let mut center_zone = CenterZoneState::default();
        let chat = TabContent::empty_chat();
        let first = center_zone.open(chat.clone(), "Chat".to_string(), true);
        let second = center_zone.open_in(
            PaneId::Secondary,
            chat.clone(),
            "Chat".to_string(),
            true,
            SplitRatio::default(),
        );
        assert_eq!(first, second);
        assert_eq!(center_zone.occurrences(&chat).len(), 1);
        assert!(!center_zone.is_split());
    }

    #[test]
    fn duplicate_occurrences_have_independent_scroll_state() {
        let owner = leptos::reactive::owner::Owner::new();
        owner.with(|| {
            let state = AppState::new();
            let key = resource_key("h", "p", test_path("scroll"));
            let primary = install_loaded_file(&state, PaneId::Primary, key);
            let secondary = state
                .duplicate_file_at_result(OpenTarget::Beside, primary)
                .tab_id()
                .expect("duplicate");
            state.save_tab_scroll_state(
                primary,
                TabScrollState {
                    scroll_top: 10,
                    scroll_height: 100,
                    client_height: 20,
                    user_scrolled_up: true,
                },
            );
            state.save_tab_scroll_state(
                secondary,
                TabScrollState {
                    scroll_top: 70,
                    scroll_height: 100,
                    client_height: 20,
                    user_scrolled_up: false,
                },
            );
            assert_ne!(
                state.tab_scroll_state_untracked(primary),
                state.tab_scroll_state_untracked(secondary)
            );
        });
    }

    #[test]
    fn closing_one_of_two_occurrences_keeps_contents_and_subscription() {
        let owner = leptos::reactive::owner::Owner::new();
        owner.with(|| {
            let state = AppState::new();
            let key = resource_key("h", "p", test_path("shared"));
            let primary = install_loaded_file(&state, PaneId::Primary, key.clone());
            let secondary = state
                .duplicate_file_at_result(OpenTarget::Beside, primary)
                .tab_id()
                .expect("duplicate");
            let intel_key = CodeIntelKey {
                host_id: key.host_id.clone(),
                project_id: key.project_id.clone(),
                path: key.path.clone(),
            };
            state.code_intel.update(|map| {
                map.insert(intel_key.clone(), CodeIntelFileState::default());
            });

            state.close_tab(primary);
            assert!(
                state
                    .open_files
                    .with_untracked(|files| files.contains_key(&key))
            );
            assert!(
                state
                    .code_intel
                    .with_untracked(|map| map.contains_key(&intel_key))
            );
            assert!(
                state
                    .center_zone
                    .with_untracked(|center_zone| center_zone.tab(secondary).is_some())
            );

            state.close_tab(secondary);
            assert!(
                !state
                    .open_files
                    .with_untracked(|files| files.contains_key(&key))
            );
            assert!(
                !state
                    .code_intel
                    .with_untracked(|map| map.contains_key(&intel_key))
            );
        });
    }

    #[test]
    fn close_all_tabs_with_two_occurrences_releases_exactly_once() {
        let owner = leptos::reactive::owner::Owner::new();
        owner.with(|| {
            let state = AppState::new();
            let key = resource_key("h", "p", test_path("bulk"));
            let primary = install_loaded_file(&state, PaneId::Primary, key.clone());
            let secondary = state
                .duplicate_file_at_result(OpenTarget::Beside, primary)
                .tab_id()
                .expect("duplicate");
            let doomed = HashSet::from([primary, secondary]);
            let (survivors, released) = state.backing_release_projection(&doomed);
            assert!(survivors.is_empty());
            assert_eq!(
                released,
                HashSet::from([BackingResource::File(key.clone())])
            );
            state.close_tabs(doomed);
            assert!(
                !state
                    .open_files
                    .with_untracked(|files| files.contains_key(&key))
            );
        });
    }

    #[test]
    fn closing_file_in_one_project_keeps_same_path_code_intel_in_another() {
        let owner = leptos::reactive::owner::Owner::new();
        owner.with(|| {
            let state = AppState::new();
            let path = test_path("same");
            let key_a = resource_key("h", "a", path.clone());
            let key_b = resource_key("h", "b", path);
            let tab_b = install_loaded_file(&state, PaneId::Primary, key_b.clone());
            state.open_files.update(|files| {
                files.insert(
                    key_a.clone(),
                    OpenFile {
                        path: key_a.path.clone(),
                        version: ProjectFileVersion(1),
                        contents: Some("a".to_string()),
                        is_binary: false,
                        missing: false,
                    },
                );
            });
            let intel_a = CodeIntelKey {
                host_id: key_a.host_id.clone(),
                project_id: key_a.project_id.clone(),
                path: key_a.path.clone(),
            };
            let intel_b = CodeIntelKey {
                host_id: key_b.host_id.clone(),
                project_id: key_b.project_id.clone(),
                path: key_b.path.clone(),
            };
            state.code_intel.update(|map| {
                map.insert(intel_a.clone(), CodeIntelFileState::default());
                map.insert(intel_b.clone(), CodeIntelFileState::default());
            });

            state.close_tab(tab_b);
            assert!(
                state
                    .code_intel
                    .with_untracked(|map| map.contains_key(&intel_a))
            );
            assert!(
                !state
                    .code_intel
                    .with_untracked(|map| map.contains_key(&intel_b))
            );
            assert!(
                state
                    .open_files
                    .with_untracked(|files| files.contains_key(&key_a))
            );
        });
    }

    #[test]
    fn goto_line_and_offset_target_only_one_occurrence() {
        let owner = leptos::reactive::owner::Owner::new();
        owner.with(|| {
            let state = AppState::new();
            let key = resource_key("h", "p", test_path("goto"));
            let primary = install_loaded_file(&state, PaneId::Primary, key);
            let secondary = state
                .duplicate_file_at_result(OpenTarget::Beside, primary)
                .tab_id()
                .expect("duplicate");
            state.target_file_navigation(primary, PendingFileNavigation::Line(12));
            assert_eq!(state.pending_goto_line.get_untracked(), Some((primary, 12)));
            assert_ne!(
                state.pending_goto_line.get_untracked(),
                Some((secondary, 12))
            );
            state.target_file_navigation(secondary, PendingFileNavigation::Offset(44));
            assert_eq!(
                state.pending_goto_offset.get_untracked(),
                Some((secondary, 44))
            );
        });
    }

    #[test]
    fn current_file_occurrence_requires_exact_tab_resource_and_version() {
        let owner = leptos::reactive::owner::Owner::new();
        owner.with(|| {
            let state = AppState::new();
            let key = resource_key("h", "p", test_path("current"));
            let other_project = resource_key("h", "other", key.path.clone());
            let primary = install_loaded_file(&state, PaneId::Primary, key.clone());
            let secondary = state
                .duplicate_file_at_result(OpenTarget::Beside, primary)
                .tab_id()
                .expect("duplicate");

            assert!(state.file_occurrence_is_current(primary, &key, ProjectFileVersion(1)));
            assert!(state.file_occurrence_is_current(secondary, &key, ProjectFileVersion(1)));
            assert!(!state.file_occurrence_is_current(
                primary,
                &other_project,
                ProjectFileVersion(1)
            ));
            assert!(!state.file_occurrence_is_current(primary, &key, ProjectFileVersion(2)));
        });
    }

    #[test]
    fn duplicate_occurrence_navigation_targets_only_the_requested_side() {
        let owner = leptos::reactive::owner::Owner::new();
        owner.with(|| {
            let state = AppState::new();
            let key = resource_key("h", "p", test_path("side-navigation"));
            let primary = install_loaded_file(&state, PaneId::Primary, key.clone());
            let secondary = state
                .duplicate_file_at_result(OpenTarget::Beside, primary)
                .tab_id()
                .expect("duplicate");

            state.target_file_navigation(secondary, PendingFileNavigation::Offset(81));

            assert_eq!(
                state.file_occurrence_in(PaneId::Primary, &key),
                Some(primary)
            );
            assert_eq!(
                state.file_occurrence_in(PaneId::Secondary, &key),
                Some(secondary)
            );
            assert_eq!(
                state.pending_goto_offset.get_untracked(),
                Some((secondary, 81))
            );
            assert_ne!(
                state.pending_goto_offset.get_untracked(),
                Some((primary, 81))
            );
        });
    }

    #[test]
    fn closing_hover_owner_clears_only_that_occurrence_context() {
        let owner = leptos::reactive::owner::Owner::new();
        owner.with(|| {
            let state = AppState::new();
            let key = resource_key("h", "p", test_path("hover-owner"));
            let primary = install_loaded_file(&state, PaneId::Primary, key.clone());
            let secondary = state
                .duplicate_file_at_result(OpenTarget::Beside, primary)
                .tab_id()
                .expect("duplicate");
            state.code_intel_hover.set(Some(HoverPopover {
                hover_id: 3,
                tab: secondary,
                key,
                version: ProjectFileVersion(1),
                offset: 2,
                anchor_left: 1.0,
                anchor_top: 2.0,
                anchor_bottom: 3.0,
                contents: Some("hover".to_owned()),
            }));

            state.close_tab(primary);
            assert!(
                state
                    .code_intel_hover
                    .with_untracked(|hover| hover.is_some())
            );
            state.close_tab(secondary);
            assert!(
                state
                    .code_intel_hover
                    .with_untracked(|hover| hover.is_none())
            );
        });
    }

    #[test]
    fn user_open_supersedes_refresh_and_refresh_never_supersedes_open() {
        let owner = leptos::reactive::owner::Owner::new();
        owner.with(|| {
            let state = AppState::new();
            let key = resource_key("h", "p", test_path("pending"));
            state.record_pending_file_open(key.clone(), PendingFileOpen::RefreshInPlace);
            let open = PendingFileOpen::Open {
                destination: PendingOpenDestination::new(PaneId::Secondary),
                navigation: Some(PendingFileNavigation::Line(9)),
            };
            state.record_pending_file_open(key.clone(), open);
            state.record_pending_file_open(key.clone(), PendingFileOpen::RefreshInPlace);
            assert_eq!(state.take_pending_file_open(&key), Some(open));
        });
    }

    #[test]
    fn pending_open_destination_survives_focus_change() {
        let owner = leptos::reactive::owner::Owner::new();
        owner.with(|| {
            let state = AppState::new();
            let key = resource_key("h", "p", test_path("destination"));
            state.record_pending_file_open(
                key.clone(),
                PendingFileOpen::Open {
                    destination: PendingOpenDestination::new(PaneId::Secondary),
                    navigation: None,
                },
            );
            state.focus_pane(PaneId::Primary);
            let Some(PendingFileOpen::Open { destination, .. }) =
                state.take_pending_file_open(&key)
            else {
                panic!("open intent retained");
            };
            assert_eq!(destination.pane(), PaneId::Secondary);
        });
    }

    #[test]
    fn active_agent_and_pending_member_follow_composer_owner() {
        let owner = leptos::reactive::owner::Owner::new();
        owner.with(|| {
            let state = AppState::new();
            let file = resource_key("h", "p", test_path("composer"));
            let file_tab = install_loaded_file(&state, PaneId::Primary, file);
            let member_id = TeamMemberId("member".to_string());
            let draft = state
                .open_tab_in(
                    PaneId::Secondary,
                    TabContent::team_member_draft("h".to_string(), member_id.clone()),
                    "Member".to_string(),
                    true,
                )
                .expect("draft opens beside");
            state.activate_tab(file_tab);
            assert_eq!(
                state
                    .center_zone
                    .with_untracked(CenterZoneState::composer_owner),
                Some((PaneId::Secondary, draft))
            );
            assert_eq!(
                state.composer_pending_team_member_untracked(),
                Some(PendingTeamMember {
                    host_id: "h".to_string(),
                    member_id,
                })
            );
        });
    }

    #[test]
    fn project_switch_round_trips_split_layout_and_ratio() {
        let owner = leptos::reactive::owner::Owner::new();
        owner.with(|| {
            let state = AppState::new();
            let project_a = ActiveProjectRef {
                host_id: "h".to_string(),
                project_id: ProjectId("a".to_string()),
            };
            let project_b = ActiveProjectRef {
                host_id: "h".to_string(),
                project_id: ProjectId("b".to_string()),
            };
            state.switch_active_project(Some(project_a.clone()));
            install_loaded_file(
                &state,
                PaneId::Primary,
                resource_key("h", "a", test_path("memory")),
            );
            state
                .open_tab_in(
                    PaneId::Secondary,
                    TabContent::chat_with_agent(ActiveAgentRef {
                        host_id: "h".to_string(),
                        agent_id: AgentId("memory-agent".to_string()),
                    }),
                    "Chat".to_string(),
                    true,
                )
                .expect("split");
            state.set_split_ratio(SplitRatio::new(0.7));

            state.switch_active_project(Some(project_b));
            assert!(!state.center_zone.with_untracked(CenterZoneState::is_split));
            state.switch_active_project(Some(project_a));
            assert!(state.center_zone.with_untracked(CenterZoneState::is_split));
            assert_eq!(
                state
                    .center_zone
                    .with_untracked(CenterZoneState::split_ratio)
                    .map(SplitRatio::get),
                Some(0.7)
            );
        });
    }

    #[test]
    fn active_agent_follows_chat_pane_when_file_pane_is_focused() {
        let owner = leptos::reactive::owner::Owner::new();
        owner.with(|| {
            let state = AppState::new();
            let file = install_loaded_file(
                &state,
                PaneId::Primary,
                resource_key("h", "p", test_path("agent-owner")),
            );
            let agent = ActiveAgentRef {
                host_id: "h".to_string(),
                agent_id: AgentId("agent".to_string()),
            };
            state
                .open_tab_in(
                    PaneId::Secondary,
                    TabContent::chat_with_agent(agent.clone()),
                    "Agent".to_string(),
                    true,
                )
                .expect("chat opens beside");
            state.activate_tab(file);
            assert_eq!(state.active_agent.get_untracked(), Some(agent));
        });
    }

    #[test]
    fn move_to_other_pane_preserves_tab_and_scroll_state() {
        let owner = leptos::reactive::owner::Owner::new();
        owner.with(|| {
            let state = AppState::new();
            let file = install_loaded_file(
                &state,
                PaneId::Primary,
                resource_key("h", "p", test_path("move")),
            );
            state.save_tab_scroll_state(
                file,
                TabScrollState {
                    scroll_top: 33,
                    scroll_height: 200,
                    client_height: 50,
                    user_scrolled_up: true,
                },
            );
            state
                .open_tab_in(
                    PaneId::Secondary,
                    TabContent::empty_chat(),
                    "Chat".to_string(),
                    true,
                )
                .expect("split");
            assert!(matches!(
                state.move_tab_to(PaneId::Secondary, file),
                MoveTabResult::Moved { tab, .. } if tab == file
            ));
            assert_eq!(
                state
                    .center_zone
                    .with_untracked(|center_zone| center_zone.locate_tab(file)),
                Some(PaneId::Secondary)
            );
            assert_eq!(
                state
                    .tab_scroll_state_untracked(file)
                    .map(|scroll| scroll.scroll_top),
                Some(33)
            );
        });
    }

    #[test]
    fn split_tab_to_left_places_dragged_tab_in_primary_pane() {
        let owner = leptos::reactive::owner::Owner::new();
        owner.with(|| {
            let state = AppState::new();
            let first = install_loaded_file(
                &state,
                PaneId::Primary,
                resource_key("h", "p", test_path("first")),
            );
            let second = install_loaded_file(
                &state,
                PaneId::Primary,
                resource_key("h", "p", test_path("second")),
            );

            assert!(matches!(
                state.split_tab_to(PaneId::Primary, second),
                MoveTabResult::Moved {
                    tab,
                    source: PaneId::Primary,
                    target: PaneId::Primary,
                } if tab == second
            ));
            state.center_zone.with_untracked(|center_zone| {
                assert!(center_zone.is_split());
                assert_eq!(center_zone.locate_tab(second), Some(PaneId::Primary));
                assert_eq!(center_zone.locate_tab(first), Some(PaneId::Secondary));
                assert_eq!(center_zone.focused_id(), PaneId::Primary);
            });
        });
    }

    #[test]
    fn reveal_tab_moves_pane_focus_but_set_active_tab_in_pane_does_not() {
        let owner = leptos::reactive::owner::Owner::new();
        owner.with(|| {
            let state = AppState::new();
            let file = install_loaded_file(
                &state,
                PaneId::Primary,
                resource_key("h", "p", test_path("focus-owner")),
            );
            let first_chat = state
                .open_tab_in(
                    PaneId::Secondary,
                    TabContent::chat_with_agent(ActiveAgentRef {
                        host_id: "h".to_owned(),
                        agent_id: AgentId("first".to_owned()),
                    }),
                    "First".to_owned(),
                    true,
                )
                .expect("first chat");
            let second_chat = state
                .open_tab_in(
                    PaneId::Secondary,
                    TabContent::chat_with_agent(ActiveAgentRef {
                        host_id: "h".to_owned(),
                        agent_id: AgentId("second".to_owned()),
                    }),
                    "Second".to_owned(),
                    true,
                )
                .expect("second chat");
            assert!(state.reveal_tab(file));

            assert!(state.set_active_tab_in_pane(PaneId::Secondary, first_chat));
            let before_update = state.center_zone.with_untracked(|center_zone| {
                (
                    center_zone.focused_id(),
                    center_zone
                        .panes()
                        .map(|(pane, state)| {
                            (
                                pane,
                                state.active_tab_id,
                                state.tabs.iter().map(|tab| tab.id).collect::<Vec<_>>(),
                            )
                        })
                        .collect::<Vec<_>>(),
                )
            });
            state.center_zone.with_untracked(|center_zone| {
                assert_eq!(center_zone.focused_id(), PaneId::Primary);
                assert_eq!(
                    center_zone.pane_active_tab_id(PaneId::Secondary),
                    Some(first_chat)
                );
            });

            assert!(state.update_tab(
                first_chat,
                TabContent::chat_with_agent(ActiveAgentRef {
                    host_id: "h".to_owned(),
                    agent_id: AgentId("upgraded".to_owned()),
                }),
                "Upgraded".to_owned(),
            ));
            state.center_zone.with_untracked(|center_zone| {
                assert_eq!(center_zone.focused_id(), PaneId::Primary);
                assert_eq!(center_zone.pane_active_tab_id(PaneId::Primary), Some(file));
                assert_eq!(
                    center_zone.pane_active_tab_id(PaneId::Secondary),
                    Some(first_chat)
                );
                assert_eq!(center_zone.locate_tab(second_chat), Some(PaneId::Secondary));
                assert_eq!(
                    (
                        center_zone.focused_id(),
                        center_zone
                            .panes()
                            .map(|(pane, state)| {
                                (
                                    pane,
                                    state.active_tab_id,
                                    state.tabs.iter().map(|tab| tab.id).collect::<Vec<_>>(),
                                )
                            })
                            .collect::<Vec<_>>(),
                    ),
                    before_update,
                    "mutation-only updates preserve focus, both selections, and strip order"
                );
            });

            assert!(state.reveal_tab(first_chat));
            assert_eq!(
                state
                    .center_zone
                    .with_untracked(CenterZoneState::focused_id),
                PaneId::Secondary
            );
        });
    }

    #[test]
    fn move_conflict_returns_authoritative_reason_without_mutation() {
        let owner = leptos::reactive::owner::Owner::new();
        owner.with(|| {
            let state = AppState::new();
            let primary = install_loaded_file(
                &state,
                PaneId::Primary,
                resource_key("h", "p", test_path("move-conflict")),
            );
            let secondary = state
                .duplicate_file_in_result(PaneId::Secondary, primary)
                .tab_id()
                .expect("duplicate occurrence");
            assert!(state.reveal_tab(primary));
            let before = state.center_zone.with_untracked(|center_zone| {
                (
                    center_zone.focused_id(),
                    center_zone
                        .panes()
                        .map(|(pane, state)| {
                            (
                                pane,
                                state.active_tab_id,
                                state.tabs.iter().map(|tab| tab.id).collect::<Vec<_>>(),
                            )
                        })
                        .collect::<Vec<_>>(),
                )
            });

            assert_eq!(
                state.move_tab_eligibility(PaneId::Secondary, primary),
                MoveTabEligibility::ResourceAlreadyInTarget {
                    existing: secondary
                }
            );
            let result = state.move_tab_to(PaneId::Secondary, primary);
            assert_eq!(
                result,
                MoveTabResult::ResourceAlreadyInTarget {
                    existing: secondary
                }
            );
            assert_eq!(
                result.disabled_reason(),
                Some(MOVE_RESOURCE_ALREADY_IN_TARGET_REASON)
            );
            let after = state.center_zone.with_untracked(|center_zone| {
                (
                    center_zone.focused_id(),
                    center_zone
                        .panes()
                        .map(|(pane, state)| {
                            (
                                pane,
                                state.active_tab_id,
                                state.tabs.iter().map(|tab| tab.id).collect::<Vec<_>>(),
                            )
                        })
                        .collect::<Vec<_>>(),
                )
            });
            assert_eq!(after, before);
        });
    }

    #[test]
    fn move_refusal_conversion_rejects_success_and_preserves_refusal_data() {
        let moved = MoveTabResult::Moved {
            tab: TabId(10),
            source: PaneId::Primary,
            target: PaneId::Secondary,
        };
        assert_eq!(MoveTabRefusal::try_from(moved), Err(moved));
        assert_eq!(
            MoveTabRefusal::try_from(MoveTabResult::SourceTabMissing),
            Ok(MoveTabRefusal::SourceTabMissing)
        );
        assert_eq!(
            MoveTabRefusal::try_from(MoveTabResult::AlreadyInTargetPane),
            Ok(MoveTabRefusal::AlreadyInTargetPane)
        );
        assert_eq!(
            MoveTabRefusal::try_from(MoveTabResult::ResourceAlreadyInTarget {
                existing: TabId(42),
            }),
            Ok(MoveTabRefusal::ResourceAlreadyInTarget {
                existing: TabId(42),
            })
        );
    }

    #[test]
    fn side_open_and_move_refusal_reasons_use_canonical_constants() {
        let refusals = [
            (MoveTabRefusal::SourceTabMissing, TAB_SOURCE_MISSING_REASON),
            (
                MoveTabRefusal::AlreadyInTargetPane,
                MOVE_ALREADY_IN_TARGET_PANE_REASON,
            ),
            (
                MoveTabRefusal::ResourceAlreadyInTarget {
                    existing: TabId(77),
                },
                MOVE_RESOURCE_ALREADY_IN_TARGET_REASON,
            ),
        ];
        for (refusal, reason) in refusals {
            assert_eq!(refusal.disabled_reason(), reason);
            assert_eq!(
                AgentOpenToSideResult::MoveRefused(refusal).disabled_reason(),
                Some(reason)
            );
            assert_eq!(
                DiffOpenToSideResult::MoveRefused(refusal).disabled_reason(),
                Some(reason)
            );
        }
        assert_eq!(
            MoveTabEligibility::AlreadyInTargetPane.disabled_reason(),
            Some(MOVE_ALREADY_IN_TARGET_PANE_REASON)
        );
        assert_eq!(
            MoveTabResult::AlreadyInTargetPane.disabled_reason(),
            Some(MOVE_ALREADY_IN_TARGET_PANE_REASON)
        );
        assert_eq!(
            AgentOpenToSideResult::TabsDisabled.disabled_reason(),
            Some(CENTER_TABS_DISABLED_REASON)
        );
        assert_eq!(
            AgentOpenToSideResult::CrossProject.disabled_reason(),
            Some(AGENT_OPEN_TO_SIDE_CROSS_PROJECT_REASON)
        );
        assert_eq!(
            AgentOpenToSideResult::NothingWouldRemain.disabled_reason(),
            Some(OPEN_TO_SIDE_NOTHING_WOULD_REMAIN_REASON)
        );
    }

    #[test]
    fn agent_open_to_side_opens_moves_and_reveals_without_duplication() {
        let owner = leptos::reactive::owner::Owner::new();
        owner.with(|| {
            let project = ActiveProjectRef {
                host_id: "h".to_owned(),
                project_id: ProjectId("p".to_owned()),
            };
            let state = AppState::new();
            state.active_project.set(Some(project.clone()));
            let file = install_loaded_file(
                &state,
                PaneId::Primary,
                resource_key("h", "p", test_path("agent-side")),
            );
            let agent = ActiveAgentRef {
                host_id: "h".to_owned(),
                agent_id: AgentId("agent".to_owned()),
            };
            let before_query = state.center_zone.with_untracked(center_snapshot);
            assert_eq!(
                state.agent_open_to_side_eligibility(&agent, Some(&project)),
                None
            );
            assert_eq!(
                state.center_zone.with_untracked(center_snapshot),
                before_query
            );
            let opened = state.open_agent_chat_to_side(
                agent.clone(),
                Some(project.clone()),
                "Agent".to_owned(),
            );
            let AgentOpenToSideResult::Opened {
                tab,
                pane: PaneId::Secondary,
            } = opened
            else {
                panic!("agent should open directly beside the file: {opened:?}");
            };
            assert_eq!(
                state.center_zone.with_untracked(|center_zone| {
                    center_zone.occurrences(&TabContent::chat_with_agent(agent.clone()))
                }),
                vec![(PaneId::Secondary, tab)]
            );

            assert!(state.reveal_tab(file));
            let before_reveal_query = state.center_zone.with_untracked(center_snapshot);
            assert_eq!(
                state.agent_open_to_side_eligibility(&agent, Some(&project)),
                None
            );
            assert_eq!(
                state.center_zone.with_untracked(center_snapshot),
                before_reveal_query
            );
            assert_eq!(
                state.open_agent_chat_to_side(agent.clone(), Some(project), "Agent".to_owned(),),
                AgentOpenToSideResult::Revealed {
                    tab,
                    pane: PaneId::Secondary,
                }
            );
            assert_eq!(
                state.center_zone.with_untracked(|center_zone| {
                    center_zone.occurrences(&TabContent::chat_with_agent(agent))
                }),
                vec![(PaneId::Secondary, tab)]
            );
        });
    }

    #[test]
    fn agent_open_to_side_moves_existing_chat_with_same_tab_id() {
        let owner = leptos::reactive::owner::Owner::new();
        owner.with(|| {
            let state = AppState::new();
            let agent = ActiveAgentRef {
                host_id: "h".to_owned(),
                agent_id: AgentId("move-agent".to_owned()),
            };
            state.open_tab(
                TabContent::chat_with_agent(agent.clone()),
                "Agent".to_owned(),
                true,
            );
            let tab = state
                .center_zone
                .with_untracked(|center_zone| {
                    center_zone.find_tab(&TabContent::chat_with_agent(agent.clone()))
                })
                .expect("chat tab");

            let before_query = state.center_zone.with_untracked(center_snapshot);
            assert_eq!(state.agent_open_to_side_eligibility(&agent, None), None);
            assert_eq!(
                state.center_zone.with_untracked(center_snapshot),
                before_query
            );

            assert_eq!(
                state.open_agent_chat_to_side(agent.clone(), None, "Agent".to_owned()),
                AgentOpenToSideResult::Moved {
                    tab,
                    source: PaneId::Primary,
                    target: PaneId::Secondary,
                }
            );
            state.center_zone.with_untracked(|center_zone| {
                assert_eq!(center_zone.locate_tab(tab), Some(PaneId::Secondary));
                assert_eq!(
                    center_zone.occurrences(&TabContent::chat_with_agent(agent)),
                    vec![(PaneId::Secondary, tab)]
                );
            });
        });
    }

    #[test]
    fn agent_open_to_side_eligibility_is_non_mutating_and_matches_refusals() {
        let owner = leptos::reactive::owner::Owner::new();
        owner.with(|| {
            let state = AppState::new();
            let agent = ActiveAgentRef {
                host_id: "h".to_owned(),
                agent_id: AgentId("blocked-agent".to_owned()),
            };
            let chat = make_tab(TabContent::chat_with_agent(agent.clone()), "Agent", true);
            let tab = chat.id;
            state.center_zone.set(CenterZoneState {
                layout: CenterLayout::Single(PaneState {
                    tabs: vec![chat],
                    active_tab_id: Some(tab),
                }),
            });

            let before = state.center_zone.with_untracked(center_snapshot);
            let sole = state.agent_open_to_side_eligibility(&agent, None);
            assert_eq!(sole, Some(AgentOpenToSideResult::NothingWouldRemain));
            assert_eq!(
                sole.and_then(AgentOpenToSideResult::disabled_reason),
                Some("Nothing would be left in this pane.")
            );
            assert_eq!(state.center_zone.with_untracked(center_snapshot), before);
            assert_eq!(
                state.open_agent_chat_to_side(agent.clone(), None, "Agent".to_owned()),
                sole.expect("sole-tab refusal")
            );
            assert_eq!(state.center_zone.with_untracked(center_snapshot), before);

            state.tabs_enabled.set(false);
            let tabs_disabled = state.agent_open_to_side_eligibility(&agent, None);
            assert_eq!(tabs_disabled, Some(AgentOpenToSideResult::TabsDisabled));
            assert_eq!(
                tabs_disabled.and_then(AgentOpenToSideResult::disabled_reason),
                Some("Enable tabs to use split view.")
            );
            assert_eq!(state.center_zone.with_untracked(center_snapshot), before);
            assert_eq!(
                state.open_agent_chat_to_side(agent.clone(), None, "Agent".to_owned()),
                tabs_disabled.expect("tabs-disabled refusal")
            );
            assert_eq!(state.center_zone.with_untracked(center_snapshot), before);

            state.tabs_enabled.set(true);
            state.active_project.set(Some(ActiveProjectRef {
                host_id: "h".to_owned(),
                project_id: ProjectId("active".to_owned()),
            }));
            let other = ActiveProjectRef {
                host_id: "h".to_owned(),
                project_id: ProjectId("other".to_owned()),
            };
            let cross_project = state.agent_open_to_side_eligibility(&agent, Some(&other));
            assert_eq!(cross_project, Some(AgentOpenToSideResult::CrossProject));
            assert_eq!(
                cross_project.and_then(AgentOpenToSideResult::disabled_reason),
                Some(AGENT_OPEN_TO_SIDE_CROSS_PROJECT_REASON)
            );
            assert_eq!(state.center_zone.with_untracked(center_snapshot), before);
            assert_eq!(
                state.open_agent_chat_to_side(agent, Some(other), "Agent".to_owned()),
                cross_project.expect("cross-project refusal")
            );
            assert_eq!(state.center_zone.with_untracked(center_snapshot), before);
        });
    }

    #[test]
    fn agent_open_to_side_refuses_sole_tab_and_cross_project() {
        let owner = leptos::reactive::owner::Owner::new();
        owner.with(|| {
            let state = AppState::new();
            let agent = ActiveAgentRef {
                host_id: "h".to_owned(),
                agent_id: AgentId("sole-agent".to_owned()),
            };
            let chat = make_tab(TabContent::chat_with_agent(agent.clone()), "Agent", true);
            let tab = chat.id;
            state.center_zone.set(CenterZoneState {
                layout: CenterLayout::Single(PaneState {
                    tabs: vec![chat],
                    active_tab_id: Some(tab),
                }),
            });
            let before = state
                .center_zone
                .with_untracked(CenterZoneState::all_tab_ids);
            let sole = state.open_agent_chat_to_side(agent.clone(), None, "Agent".to_owned());
            assert_eq!(sole, AgentOpenToSideResult::NothingWouldRemain);
            assert_eq!(
                sole.disabled_reason(),
                Some("Nothing would be left in this pane.")
            );
            assert_eq!(
                state
                    .center_zone
                    .with_untracked(CenterZoneState::all_tab_ids),
                before
            );

            let active = ActiveProjectRef {
                host_id: "h".to_owned(),
                project_id: ProjectId("active".to_owned()),
            };
            state.active_project.set(Some(active));
            state.tabs_enabled.set(false);
            let tabs_disabled =
                state.open_agent_chat_to_side(agent.clone(), None, "Agent".to_owned());
            assert_eq!(tabs_disabled, AgentOpenToSideResult::TabsDisabled);
            assert_eq!(
                tabs_disabled.disabled_reason(),
                Some("Enable tabs to use split view.")
            );
            state.tabs_enabled.set(true);
            let other = ActiveProjectRef {
                host_id: "h".to_owned(),
                project_id: ProjectId("other".to_owned()),
            };
            let cross_project =
                state.open_agent_chat_to_side(agent, Some(other), "Agent".to_owned());
            assert_eq!(cross_project, AgentOpenToSideResult::CrossProject);
            assert_eq!(
                cross_project.disabled_reason(),
                Some(AGENT_OPEN_TO_SIDE_CROSS_PROJECT_REASON)
            );
            assert_eq!(
                state
                    .center_zone
                    .with_untracked(CenterZoneState::all_tab_ids),
                before
            );
        });
    }

    #[test]
    fn diff_open_to_side_opens_reveals_and_moves_the_exact_occurrence() {
        let owner = leptos::reactive::owner::Owner::new();
        owner.with(|| {
            let project = ActiveProjectRef {
                host_id: "h".to_owned(),
                project_id: ProjectId("p".to_owned()),
            };
            let state = AppState::new();
            state.active_project.set(Some(project.clone()));
            let home = state
                .center_zone
                .with_untracked(|center_zone| center_zone.active_tab_id())
                .expect("home tab");
            let key = DiffKey::new(
                "h",
                project.project_id,
                ProjectRootPath("/repo".to_owned()),
                ProjectDiffScope::Unstaged,
                "src/lib.rs",
            );
            let content = key.tab_content();

            let before_query = state.center_zone.with_untracked(center_snapshot);
            assert_eq!(state.diff_open_to_side_eligibility(&key), None);
            assert_eq!(
                state.center_zone.with_untracked(center_snapshot),
                before_query
            );
            let opened = state.open_diff_to_side(key.clone(), "Diff".to_owned());
            let DiffOpenToSideResult::Opened {
                tab,
                pane: PaneId::Secondary,
            } = opened
            else {
                panic!("diff should open in the opposite pane: {opened:?}");
            };
            assert_eq!(
                state
                    .center_zone
                    .with_untracked(|center_zone| center_zone.occurrences(&content)),
                vec![(PaneId::Secondary, tab)]
            );

            assert!(state.reveal_tab(home));
            assert_eq!(
                state.open_diff_to_side(key.clone(), "Ignored label".to_owned()),
                DiffOpenToSideResult::Revealed {
                    tab,
                    pane: PaneId::Secondary,
                }
            );
            assert_eq!(
                state
                    .center_zone
                    .with_untracked(|center_zone| center_zone.occurrences(&content)),
                vec![(PaneId::Secondary, tab)]
            );

            state
                .open_tab_in(
                    PaneId::Secondary,
                    TabContent::AgentMonitor,
                    "Agents".to_owned(),
                    true,
                )
                .expect("source-pane survivor");
            assert!(state.reveal_tab(tab));
            let before_move_query = state.center_zone.with_untracked(center_snapshot);
            assert_eq!(state.diff_open_to_side_eligibility(&key), None);
            assert_eq!(
                state.center_zone.with_untracked(center_snapshot),
                before_move_query
            );
            assert_eq!(
                state.open_diff_to_side(key, "Ignored label".to_owned()),
                DiffOpenToSideResult::Moved {
                    tab,
                    source: PaneId::Secondary,
                    target: PaneId::Primary,
                }
            );
            state.center_zone.with_untracked(|center_zone| {
                assert_eq!(center_zone.locate_tab(tab), Some(PaneId::Primary));
                assert_eq!(
                    center_zone.occurrences(&content),
                    vec![(PaneId::Primary, tab)]
                );
            });
        });
    }

    #[test]
    fn diff_open_to_side_eligibility_matches_typed_refusals_without_mutation() {
        let owner = leptos::reactive::owner::Owner::new();
        owner.with(|| {
            let project = ActiveProjectRef {
                host_id: "h".to_owned(),
                project_id: ProjectId("p".to_owned()),
            };
            let state = AppState::new();
            let key = DiffKey::new(
                "h",
                project.project_id.clone(),
                ProjectRootPath("/repo".to_owned()),
                ProjectDiffScope::Staged,
                "src/lib.rs",
            );

            state.active_project.set(Some(ActiveProjectRef {
                host_id: "h".to_owned(),
                project_id: ProjectId("other".to_owned()),
            }));
            let before_cross_project = state.center_zone.with_untracked(center_snapshot);
            let cross_project = state.diff_open_to_side_eligibility(&key);
            assert_eq!(cross_project, Some(DiffOpenToSideResult::CrossProject));
            assert_eq!(
                cross_project.and_then(DiffOpenToSideResult::disabled_reason),
                Some(OPEN_TO_SIDE_CROSS_PROJECT_REASON)
            );
            assert_eq!(
                state.open_diff_to_side(key.clone(), "Diff".to_owned()),
                cross_project.expect("cross-project refusal")
            );
            assert_eq!(
                state.center_zone.with_untracked(center_snapshot),
                before_cross_project
            );

            state.active_project.set(Some(project));
            let diff = make_tab(key.tab_content(), "Diff", true);
            let tab = diff.id;
            state.center_zone.set(CenterZoneState {
                layout: CenterLayout::Single(PaneState {
                    tabs: vec![diff],
                    active_tab_id: Some(tab),
                }),
            });
            let before_sole = state.center_zone.with_untracked(center_snapshot);
            let sole = state.diff_open_to_side_eligibility(&key);
            assert_eq!(sole, Some(DiffOpenToSideResult::NothingWouldRemain));
            assert_eq!(
                sole.and_then(DiffOpenToSideResult::disabled_reason),
                Some(OPEN_TO_SIDE_NOTHING_WOULD_REMAIN_REASON)
            );
            assert_eq!(
                state.open_diff_to_side(key.clone(), "Diff".to_owned()),
                sole.expect("sole-tab refusal")
            );
            assert_eq!(
                state.center_zone.with_untracked(center_snapshot),
                before_sole
            );

            state.tabs_enabled.set(false);
            let tabs_disabled = state.diff_open_to_side_eligibility(&key);
            assert_eq!(tabs_disabled, Some(DiffOpenToSideResult::TabsDisabled));
            assert_eq!(
                tabs_disabled.and_then(DiffOpenToSideResult::disabled_reason),
                Some(CENTER_TABS_DISABLED_REASON)
            );
            assert_eq!(
                state.open_diff_to_side(key, "Diff".to_owned()),
                tabs_disabled.expect("tabs-disabled refusal")
            );
            assert_eq!(
                state.center_zone.with_untracked(center_snapshot),
                before_sole
            );
        });
    }

    #[test]
    fn mounted_tabs_pin_each_panes_active_tab() {
        let owner = leptos::reactive::owner::Owner::new();
        owner.with(|| {
            let state = AppState::new();
            let first = install_loaded_file(
                &state,
                PaneId::Primary,
                resource_key("h", "p", test_path("pin-a")),
            );
            let second = state
                .open_tab_in(
                    PaneId::Secondary,
                    TabContent::empty_chat(),
                    "Chat".to_string(),
                    true,
                )
                .expect("second pane");
            state.tab_lru.set(Vec::new());
            let mounted = state.mounted_tab_ids();
            assert!(mounted.contains(&first));
            assert!(mounted.contains(&second));
        });
    }

    #[test]
    fn apply_chat_message_metadata_patches_existing_row_in_place() {
        let owner = leptos::reactive::owner::Owner::new();
        owner.with(|| {
            let state = AppState::new();
            let agent_id = AgentId("a-meta".to_owned());
            let message_id = protocol::ChatMessageId("msg-1".to_owned());

            let initial = ChatMessageEntry {
                message: ChatMessage {
                    message_id: Some(message_id.clone()),
                    timestamp: 1,
                    sender: protocol::MessageSender::Assistant {
                        agent: "test-agent".to_owned(),
                    },
                    content: "hello world".to_owned(),
                    reasoning: None,
                    tool_calls: Vec::new(),
                    model_info: None,
                    token_usage: None,
                    context_breakdown: None,
                    images: None,
                },
                tool_requests: Vec::new(),
            };
            let handle = state.push_chat_entry(agent_id.clone(), initial);
            let original_row_id = handle.id;

            let update = MessageMetadataUpdateData {
                message_id: message_id.clone(),
                model_info: Some(protocol::ModelInfo {
                    model: "gpt-test".to_owned(),
                }),
                token_usage: Some(protocol::MessageTokenUsage::request_known(
                    protocol::TokenUsage {
                        input_tokens: 7,
                        output_tokens: 3,
                        total_tokens: 10,
                        cached_prompt_tokens: None,
                        cache_creation_input_tokens: None,
                        reasoning_tokens: None,
                    },
                )),
                context_breakdown: None,
            };
            state.apply_chat_message_metadata(&agent_id, update);

            let rows = state
                .chat_rows
                .with_untracked(|m| m.get(&agent_id).cloned())
                .expect("agent rows");
            assert_eq!(rows.len(), 1, "metadata update must not append a row");
            assert_eq!(rows[0].id, original_row_id, "row identity preserved");
            let entry = rows[0].entry.get_untracked();
            assert_eq!(entry.message.content, "hello world", "content untouched");
            assert!(
                entry
                    .message
                    .model_info
                    .as_ref()
                    .is_some_and(|m| m.model == "gpt-test"),
                "model_info patched"
            );
            assert!(
                entry
                    .message
                    .token_usage
                    .as_ref()
                    .and_then(|t| t.request.known_usage())
                    .is_some_and(|u| u.total_tokens == 10),
                "token_usage request scope patched"
            );
            assert!(
                entry.message.context_breakdown.is_none(),
                "None update fields leave existing value alone"
            );

            // A second update that only carries context_breakdown must
            // not stomp on the previously-patched model_info / token_usage.
            let breakdown = protocol::ContextBreakdown {
                system_prompt_bytes: 1,
                tool_io_bytes: 2,
                conversation_history_bytes: 3,
                reasoning_bytes: 4,
                context_injection_bytes: 5,
                input_tokens: 6,
                context_window: 8000,
            };
            state.apply_chat_message_metadata(
                &agent_id,
                MessageMetadataUpdateData {
                    message_id: message_id.clone(),
                    model_info: None,
                    token_usage: None,
                    context_breakdown: Some(breakdown),
                },
            );
            let entry = rows[0].entry.get_untracked();
            assert!(
                entry
                    .message
                    .model_info
                    .as_ref()
                    .is_some_and(|m| m.model == "gpt-test"),
                "prior model_info preserved across partial update"
            );
            assert!(
                entry
                    .message
                    .token_usage
                    .as_ref()
                    .and_then(|t| t.request.known_usage())
                    .is_some_and(|u| u.total_tokens == 10),
                "prior token_usage preserved across partial update"
            );
            assert!(
                entry
                    .message
                    .context_breakdown
                    .as_ref()
                    .is_some_and(|c| c.context_window == 8000),
                "context_breakdown patched"
            );

            // Unknown message id is a warning, not a crash.
            state.apply_chat_message_metadata(
                &agent_id,
                MessageMetadataUpdateData {
                    message_id: protocol::ChatMessageId("missing".to_owned()),
                    model_info: None,
                    token_usage: None,
                    context_breakdown: None,
                },
            );
            assert_eq!(
                state
                    .chat_rows
                    .with_untracked(|m| m.get(&agent_id).map(|r| r.len()).unwrap_or(0)),
                1,
                "unknown message id must not append a row"
            );
        });
    }

    #[test]
    fn pending_agent_settings_cleanup_is_request_scoped_and_fifo() {
        let owner = leptos::reactive::owner::Owner::new();
        owner.with(|| {
            let state = AppState::new();
            let project_id = ProjectId("project".to_owned());
            let first = SessionSettingsValues(HashMap::from([(
                "model".to_owned(),
                protocol::SessionSettingValue::String("first".to_owned()),
            )]));
            let second = SessionSettingsValues(HashMap::from([(
                "model".to_owned(),
                protocol::SessionSettingValue::String("second".to_owned()),
            )]));
            let first_id = state.queue_pending_agent_session_settings(
                "host".to_owned(),
                Some(project_id.clone()),
                first,
            );
            state.queue_pending_agent_session_settings(
                "host".to_owned(),
                Some(project_id.clone()),
                second.clone(),
            );

            state.discard_pending_agent_session_settings("host", Some(&project_id), first_id);

            assert_eq!(
                state.take_pending_agent_session_settings("host", Some(&project_id)),
                Some(second),
                "one failed send must not delete a later pending handoff"
            );
            assert!(
                state
                    .pending_agent_session_settings
                    .with_untracked(HashMap::is_empty)
            );

            state.queue_pending_agent_session_settings(
                "host".to_owned(),
                Some(project_id.clone()),
                SessionSettingsValues::default(),
            );
            state.queue_pending_agent_session_settings(
                "other-host".to_owned(),
                None,
                SessionSettingsValues::default(),
            );
            state.clear_host_runtime("host");
            assert!(
                state
                    .pending_agent_session_settings
                    .with_untracked(|pending| {
                        pending.keys().all(|(host_id, _)| host_id == "other-host")
                    })
            );
        });
    }

    #[test]
    fn active_project_restore_waits_for_owning_host_and_respects_new_selection() {
        let owner = leptos::reactive::owner::Owner::new();
        owner.with(|| {
            let state = AppState::new();
            let restored = ActiveProjectRef {
                host_id: "restored-host".to_owned(),
                project_id: ProjectId("restored-project".to_owned()),
            };
            state
                .pending_active_project_restore
                .set(Some(restored.clone()));
            state.projects.update(|projects| {
                projects.push(ProjectInfo {
                    host_id: restored.host_id.clone(),
                    project: Project {
                        id: restored.project_id.clone(),
                        name: "Restored".to_owned(),
                        sort_order: 0,
                        source: protocol::ProjectSource::Standalone {
                            roots: vec![ProjectRootPath("/restored".to_owned())],
                        },
                    },
                });
            });

            state.restore_active_project_after_host_bootstrap("other-host");
            assert_eq!(
                state.pending_active_project_restore.get_untracked(),
                Some(restored.clone())
            );
            state.restore_active_project_after_host_bootstrap("restored-host");
            assert_eq!(state.active_project.get_untracked(), Some(restored.clone()));

            let newly_selected = ActiveProjectRef {
                host_id: "new-host".to_owned(),
                project_id: ProjectId("new-project".to_owned()),
            };
            state.active_project.set(Some(newly_selected.clone()));
            state.pending_active_project_restore.set(Some(restored));
            state.restore_active_project_after_host_bootstrap("restored-host");
            assert_eq!(
                state.active_project.get_untracked(),
                Some(newly_selected),
                "late bootstrap must not override a newer user selection"
            );
            assert_eq!(state.pending_active_project_restore.get_untracked(), None);

            let missing_state = AppState::new();
            missing_state
                .pending_active_project_restore
                .set(Some(ActiveProjectRef {
                    host_id: "missing-host".to_owned(),
                    project_id: ProjectId("missing-project".to_owned()),
                }));
            missing_state.restore_active_project_after_host_bootstrap("missing-host");
            assert_eq!(
                missing_state.pending_active_project_restore.get_untracked(),
                None,
                "an authoritative catalog must retire a stale persisted selection"
            );
            assert_eq!(missing_state.active_project.get_untracked(), None);
        });
    }
}
