use std::cell::Cell;
use std::collections::{HashMap, HashSet};

use crate::bridge::{ConfiguredHost, RemoteHostLifecycleStatus};
use leptos::prelude::*;
use protocol::{
    AgentId, AgentOrigin, AgentWorkflowMetadata, BackendKind, BackendSetupInfo, ChatMessage,
    ChatMessageId, CustomAgent, CustomAgentId, DiffContextMode, GitBranchName, HostAbsPath,
    HostBrowseEntry, HostBrowseErrorPayload, HostPlatform, HostSettings, McpServerConfig,
    McpServerId, MessageMetadataUpdateData, MobileAccessStatePayload, MobilePairingOfferPayload,
    Project, ProjectDiffScope, ProjectGitDiffFile, ProjectGitDiffPayload, ProjectId, ProjectPath,
    ProjectRootGitStatus, ProjectRootListing, ProjectRootPath, ProjectSearchFileResult,
    QueuedMessageEntry, Review, ReviewCommentId, ReviewId, ReviewSuggestionId, ReviewSummary,
    SessionId, SessionSchemaEntry, SessionSettingsValues, SessionSummary, Skill, SkillId, Steering,
    SteeringId, StreamPath, TaskList, Team, TeamDraft, TeamDraftId, TeamId, TeamMember,
    TeamMemberBindingPayload, TeamMemberId, TeamMemberShuffleSuggestion,
    TeamMemberShuffleSuggestionNotifyPayload, TeamPresetCatalog, TerminalId,
    ToolExecutionCompletedData, ToolProgressData, ToolRequest, WorkflowDiagnostic, WorkflowRunId,
    WorkflowRunSnapshot, WorkflowSummary,
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

/// Id of the builtin "Default" custom agent. It backs every spawn that picks
/// no explicit agent, so pickers that already offer a "Default agent" row
/// hide this record to avoid a duplicate entry.
pub const DEFAULT_CUSTOM_AGENT_ID: &str = "tyde-default";

thread_local! {
    static NEXT_TAB_ID: Cell<u64> = const { Cell::new(0) };
}

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
        path: ProjectPath,
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

#[derive(Clone, Debug)]
pub struct CenterZoneState {
    pub tabs: Vec<Tab>,
    pub active_tab_id: Option<TabId>,
}

impl CenterZoneState {
    pub fn new_home() -> Self {
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

    pub fn find_tab(&self, content: &TabContent) -> Option<TabId> {
        self.tabs
            .iter()
            .find(|t| t.content == *content)
            .map(|t| t.id)
    }

    pub fn open(&mut self, content: TabContent, label: String, closeable: bool) -> TabId {
        if let Some(id) = self.find_tab(&content) {
            self.active_tab_id = Some(id);
            return id;
        }
        let id = next_tab_id();
        self.tabs.push(Tab {
            id,
            content,
            label,
            closeable,
        });
        self.active_tab_id = Some(id);
        id
    }

    pub fn activate(&mut self, id: TabId) {
        if self.tabs.iter().any(|t| t.id == id) {
            self.active_tab_id = Some(id);
        }
    }

    pub fn close(&mut self, id: TabId) {
        let Some(idx) = self.tabs.iter().position(|t| t.id == id) else {
            return;
        };
        if !self.tabs[idx].closeable {
            return;
        }
        self.tabs.remove(idx);
        if self.active_tab_id == Some(id) {
            if self.tabs.is_empty() {
                let home_id = next_tab_id();
                self.tabs.push(Tab {
                    id: home_id,
                    content: TabContent::Home,
                    label: "Home".to_string(),
                    closeable: false,
                });
                self.active_tab_id = Some(home_id);
            } else {
                self.active_tab_id = Some(self.tabs[idx.min(self.tabs.len() - 1)].id);
            }
        }
    }

    pub fn replace_active(&mut self, content: TabContent, label: String, closeable: bool) -> TabId {
        if let Some(active_id) = self.active_tab_id
            && let Some(tab) = self.tabs.iter_mut().find(|t| t.id == active_id)
        {
            tab.content = content;
            tab.label = label;
            tab.closeable = closeable;
            return active_id;
        }
        self.open(content, label, closeable)
    }

    pub fn active_tab(&self) -> Option<&Tab> {
        self.active_tab_id
            .and_then(|id| self.tabs.iter().find(|t| t.id == id))
    }

    pub fn active_content(&self) -> Option<&TabContent> {
        self.active_tab().map(|t| &t.content)
    }

    pub fn close_others(&mut self, id: TabId) {
        self.tabs.retain(|t| t.id == id || !t.closeable);
        if self.tabs.iter().any(|t| t.id == id) {
            self.active_tab_id = Some(id);
        }
    }

    pub fn close_to_right(&mut self, id: TabId) {
        let Some(idx) = self.tabs.iter().position(|t| t.id == id) else {
            return;
        };
        let mut i = self.tabs.len();
        while i > idx + 1 {
            i -= 1;
            if self.tabs[i].closeable {
                self.tabs.remove(i);
            }
        }
        if let Some(active) = self.active_tab_id
            && !self.tabs.iter().any(|t| t.id == active)
        {
            self.active_tab_id = Some(id);
        }
    }

    pub fn close_all(&mut self) {
        self.tabs.retain(|t| !t.closeable);
        if self.tabs.is_empty() {
            let home_id = next_tab_id();
            self.tabs.push(Tab {
                id: home_id,
                content: TabContent::Home,
                label: "Home".to_string(),
                closeable: false,
            });
            self.active_tab_id = Some(home_id);
        } else {
            let active_exists = self
                .active_tab_id
                .is_some_and(|a| self.tabs.iter().any(|t| t.id == a));
            if !active_exists {
                self.active_tab_id = Some(self.tabs[0].id);
            }
        }
    }

    pub fn rename_tab_label(&mut self, id: TabId, new_label: String) {
        if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == id) {
            tab.label = new_label;
        }
    }
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

#[derive(Clone, Debug)]
pub struct OpenFile {
    pub path: ProjectPath,
    pub contents: Option<String>,
    pub is_binary: bool,
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
fn close_agent_tabs_in_cz(center_zone: &mut CenterZoneState, host_id: &str, agent_id: &AgentId) {
    let remove_ids: Vec<_> = center_zone
        .tabs
        .iter()
        .filter(|tab| {
            matches!(
                &tab.content,
                TabContent::Chat { agent_ref: Some(ar), .. }
                    if ar.host_id == host_id && ar.agent_id == *agent_id
            )
        })
        .map(|tab| tab.id)
        .collect();
    for id in remove_ids {
        center_zone.close(id);
    }
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
    pub open_files: HashMap<ProjectPath, OpenFile>,
    pub diff_contents: HashMap<DiffKey, DiffViewState>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ActiveProjectRef {
    pub host_id: String,
    pub project_id: ProjectId,
}

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
    pub fn defaults_for(project: Option<&ActiveProjectRef>) -> Self {
        Self {
            hide_sub_agents: false,
            hide_inactive: false,
            show_other_projects: project.is_none(),
        }
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

#[derive(Clone, Debug, PartialEq, Eq)]
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
    /// Derived from `center_zone.active_tab_id` — when the active tab is a
    /// `Chat`, this projects its `agent_ref`; otherwise `None`. Read-only by
    /// design (philosophy doc rule against derived state stored as a signal):
    /// to change which agent is "active", change the active chat tab via
    /// `activate_tab` / `open_tab` / `close_tab`.
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
    pub center_zone: RwSignal<CenterZoneState>,
    /// Tabs whose content components are currently mounted, MRU-first. The
    /// active tab is always at the front; the next slot (if any) is the most
    /// recently active tab before it. Tabs absent from this list have their
    /// content unmounted entirely — no DOM, no reactive subscriptions. This
    /// keeps "many tabs open" cheap: we pay for at most `TAB_LRU_CAPACITY`
    /// component trees regardless of how many tabs the user has opened.
    /// Driven by an Effect in `App` watching `center_zone.active_tab_id`.
    pub tab_lru: RwSignal<Vec<TabId>>,
    pub tab_scroll_state: RwSignal<HashMap<TabId, TabScrollState>>,
    pub tabs_enabled: RwSignal<bool>,
    pub left_dock: RwSignal<DockVisibility>,
    pub right_dock: RwSignal<DockVisibility>,
    pub right_tab: RwSignal<RightTab>,
    pub bottom_dock: RwSignal<DockVisibility>,
    pub file_tree: RwSignal<HashMap<ProjectId, Vec<ProjectRootListing>>>,
    pub git_status: RwSignal<HashMap<ProjectId, Vec<ProjectRootGitStatus>>>,
    pub open_files: RwSignal<HashMap<ProjectPath, OpenFile>>,
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
    /// Bumped to request the Search panel focus (and select) its query input —
    /// e.g. on the Cmd/Ctrl+Shift+F shortcut or the "search in folder" action.
    pub search_focus_seq: RwSignal<u32>,
    /// When set, the file view for this `ProjectPath` should scroll so the
    /// given 1-based line is visible. Consumed (cleared) by the file view.
    pub pending_goto_line: RwSignal<Option<(ProjectPath, u32)>>,
    pub host_settings_by_host: RwSignal<HashMap<String, HostSettings>>,
    pub backend_setup_by_host: RwSignal<HashMap<String, Vec<BackendSetupInfo>>>,
    pub agent_message_queue: RwSignal<HashMap<AgentId, Vec<QueuedMessageEntry>>>,
    pub agent_turn_active: RwSignal<HashMap<AgentId, bool>>,
    pub draft_backend_override: RwSignal<Option<BackendKind>>,
    pub draft_custom_agent_id: RwSignal<Option<CustomAgentId>>,
    pub session_schemas: RwSignal<HashMap<String, HashMap<BackendKind, SessionSchemaEntry>>>,
    pub schemas_loaded_for_host: RwSignal<HashMap<String, bool>>,
    /// Host id for which the next `NewTerminal` should steal focus. Set when the
    /// user clicks Install/Sign-in; consumed in the dispatcher so the new
    /// terminal becomes active even if another terminal was already selected.
    pub pending_terminal_focus: RwSignal<Option<String>>,
    pub agent_session_settings: RwSignal<HashMap<AgentId, SessionSettingsValues>>,
    pub draft_session_settings: RwSignal<SessionSettingsValues>,
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
    /// Session-local manual ordering for the Agent Monitor center view.
    /// Empty means "use the derived default sort"; once the user reorders,
    /// known rows stay in this order until Reset.
    pub agent_monitor_order: RwSignal<Vec<AgentMonitorKey>>,
    pub agents_panel_filters: RwSignal<HashMap<Option<ActiveProjectRef>, AgentsPanelFilters>>,
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
        let initial_lru: Vec<TabId> = initial_center_zone.active_tab_id.into_iter().collect();
        let center_zone: RwSignal<CenterZoneState> = RwSignal::new(initial_center_zone);
        let active_agent: Memo<Option<ActiveAgentRef>> = Memo::new(move |_| {
            center_zone.with(|cz| {
                cz.active_tab().and_then(|tab| match &tab.content {
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
            active_agent,
            chat_rows: RwSignal::new(HashMap::new()),
            chat_tool_rows: RwSignal::new(HashMap::new()),
            chat_message_rows: RwSignal::new(HashMap::new()),
            streaming_text: RwSignal::new(HashMap::new()),
            tool_progress: RwSignal::new(HashMap::new()),
            chat_input: RwSignal::new(String::new()),
            task_lists: RwSignal::new(HashMap::new()),
            center_zone,
            tab_lru: RwSignal::new(initial_lru),
            tab_scroll_state: RwSignal::new(HashMap::new()),
            tabs_enabled: RwSignal::new(true),
            left_dock: RwSignal::new(DockVisibility::Visible),
            right_dock: RwSignal::new(DockVisibility::Visible),
            right_tab: RwSignal::new(RightTab::Agents),
            bottom_dock: RwSignal::new(DockVisibility::Hidden),
            file_tree: RwSignal::new(HashMap::new()),
            git_status: RwSignal::new(HashMap::new()),
            open_files: RwSignal::new(HashMap::new()),
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
            search_focus_seq: RwSignal::new(0),
            pending_goto_line: RwSignal::new(None),
            host_settings_by_host: RwSignal::new(HashMap::new()),
            backend_setup_by_host: RwSignal::new(HashMap::new()),
            agent_message_queue: RwSignal::new(HashMap::new()),
            agent_turn_active: RwSignal::new(HashMap::new()),
            draft_backend_override: RwSignal::new(None),
            draft_custom_agent_id: RwSignal::new(None),
            session_schemas: RwSignal::new(HashMap::new()),
            schemas_loaded_for_host: RwSignal::new(HashMap::new()),
            pending_terminal_focus: RwSignal::new(None),
            agent_session_settings: RwSignal::new(HashMap::new()),
            draft_session_settings: RwSignal::new(SessionSettingsValues::default()),
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
            teams: RwSignal::new(HashMap::new()),
            team_members: RwSignal::new(HashMap::new()),
            team_member_bindings: RwSignal::new(HashMap::new()),
            team_preset_catalogs: RwSignal::new(HashMap::new()),
            team_drafts: RwSignal::new(HashMap::new()),
            team_member_shuffle_suggestions: RwSignal::new(HashMap::new()),
            agent_monitor_order: RwSignal::new(Vec::new()),
            agents_panel_filters: RwSignal::new(HashMap::new()),
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
        }
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
            for tab in cz.tabs.iter_mut() {
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
            }
        });
        let prev_for_memory = prev_agent_id.clone();
        self.project_view_memory.update(|map| {
            for memory in map.values_mut() {
                let Some(cz) = memory.center_zone.as_mut() else {
                    continue;
                };
                for tab in cz.tabs.iter_mut() {
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
                }
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
    fn finalize_compaction_close(&self, host_id: &str, agent_id: &AgentId) {
        self.agents.update(|agents| {
            agents.retain(|agent| !(agent.host_id == host_id && agent.agent_id == *agent_id));
        });
        self.chat_rows.update(|map| {
            map.remove(agent_id);
        });
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
        self.agent_turn_active.update(|map| {
            map.remove(agent_id);
        });
        self.transient_events.update(|map| {
            map.remove(agent_id);
        });
        self.task_lists.update(|map| {
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
        self.center_zone
            .update(|cz| close_agent_tabs_in_cz(cz, &host_for_cz, &agent_for_cz));
        let host_for_memory = host_id.to_owned();
        let agent_for_memory = agent_id.clone();
        self.project_view_memory.update(|memories| {
            for memory in memories.values_mut() {
                if let Some(center_zone) = memory.center_zone.as_mut() {
                    close_agent_tabs_in_cz(center_zone, &host_for_memory, &agent_for_memory);
                }
            }
        });
        self.prune_tab_lru();
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
            rows.entry(agent_id.clone())
                .or_default()
                .push(handle.clone());
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
                    .insert(message_id, handle.id);
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

        // active_agent is a Memo derived from center_zone — restoring center_zone
        // implicitly restores it. Tab LRU is reset and re-seeded with the
        // incoming project's active tab so the first render after switch
        // mounts content (avoids a one-frame empty flash before the Effect
        // in `App` fires).
        match (next.is_some(), restored) {
            (true, Some(memory)) => {
                let cz = memory.center_zone.unwrap_or_default();
                self.tab_lru.set(cz.active_tab_id.into_iter().collect());
                self.center_zone.set(cz);
                self.active_terminal.set(memory.active_terminal);
                self.open_files.set(memory.open_files);
                self.diff_contents.set(memory.diff_contents);
            }
            (true, None) => {
                let mut cz = CenterZoneState::default();
                cz.open(TabContent::empty_chat(), "New Chat".to_string(), true);
                self.tab_lru.set(cz.active_tab_id.into_iter().collect());
                self.center_zone.set(cz);
                self.active_terminal.set(None);
                self.open_files.set(HashMap::new());
                self.diff_contents.set(HashMap::new());
            }
            (false, _) => {
                let cz = CenterZoneState::default();
                self.tab_lru.set(cz.active_tab_id.into_iter().collect());
                self.center_zone.set(cz);
                self.active_terminal.set(None);
                self.open_files.set(HashMap::new());
                self.diff_contents.set(HashMap::new());
            }
        }
    }

    pub fn forget_project_view_memory(&self, project: &ActiveProjectRef) {
        self.project_view_memory.update(|map| {
            map.remove(project);
        });
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
        let reviews_before = self.reviews.with_untracked(|m| m.len());
        let action_gates_before = self.review_action_pending.with_untracked(|m| m.len());
        let target_gates_before = self
            .review_action_target_pending
            .with_untracked(|s| s.len());
        let create_pending_before = self
            .review_create_pending
            .with_untracked(|m| m.iter().filter(|((h, _), _)| h == host_id).count());
        log::info!(
            "host.clear_host_runtime.start host={host_id} reviews_retained={reviews_before} action_gates_retained={action_gates_before} target_gates_retained={target_gates_before} host_create_pending={create_pending_before}"
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
        if !agent_ids.is_empty() {
            let drop_set: std::collections::HashSet<AgentId> = agent_ids.iter().cloned().collect();
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
            self.task_lists.update(|map| {
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
        self.session_schemas.update(|schemas| {
            schemas.remove(host_id);
        });
        self.schemas_loaded_for_host.update(|loaded| {
            loaded.remove(host_id);
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

        if self
            .active_project
            .get_untracked()
            .as_ref()
            .is_some_and(|active| active.host_id == host_id)
        {
            self.switch_active_project(None);
        }
        // active_agent is a Memo over center_zone; we cannot clear it directly.
        // Stale Chat tabs referencing the disconnected host are a known
        // follow-up bug (deferred from the perf plan).
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
        let tabs_enabled = self.tabs_enabled.get_untracked();
        let mut replaced_id = None;
        self.center_zone.update(|cz| {
            if tabs_enabled {
                cz.open(content, label, closeable);
            } else {
                replaced_id = Some(cz.replace_active(content, label, closeable));
            }
        });
        if let Some(id) = replaced_id {
            self.forget_tab_scroll_state(id);
        }
    }

    /// Insert `id` at the MRU front of `tab_lru`, dedup, truncate to
    /// `TAB_LRU_CAPACITY`. Called whenever a tab becomes active (via the
    /// Effect installed in `App` watching `active_tab_id`). Tabs that fall
    /// off the end of the list have their components unmounted by the
    /// next `<For>` re-render in `CenterZone`.
    pub fn bump_tab_lru(&self, id: TabId) {
        self.tab_lru.update(|lru| {
            lru.retain(|existing| *existing != id);
            lru.insert(0, id);
            if lru.len() > TAB_LRU_CAPACITY {
                lru.truncate(TAB_LRU_CAPACITY);
            }
        });
    }

    /// Remove `id` from `tab_lru` outright. Called when a tab is closed —
    /// keeps the LRU from referencing dead tab ids.
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

    /// Drop any LRU entries for tab ids no longer present in `center_zone`.
    /// Called after bulk-close operations (`close_others`,
    /// `close_tabs_to_right`, `close_all`) so we don't keep mounting
    /// references to vanished tabs.
    pub fn prune_tab_lru(&self) {
        let live: Vec<TabId> = self
            .center_zone
            .with_untracked(|cz| cz.tabs.iter().map(|t| t.id).collect());
        self.tab_lru.update(|lru| {
            lru.retain(|id| live.contains(id));
        });
    }

    pub fn close_tab(&self, id: TabId) {
        let content = self.center_zone.with_untracked(|cz| {
            cz.tabs
                .iter()
                .find(|t| t.id == id)
                .map(|t| t.content.clone())
        });
        if let Some(content) = content {
            match &content {
                TabContent::File { path } => {
                    let path = path.clone();
                    self.open_files.update(|files| {
                        files.remove(&path);
                    });
                }
                TabContent::Diff {
                    host_id,
                    project_id,
                    root,
                    scope,
                    path,
                } => {
                    let key = DiffKey::new(
                        host_id.clone(),
                        project_id.clone(),
                        root.clone(),
                        *scope,
                        path.clone(),
                    );
                    self.diff_contents.update(|diffs| {
                        diffs.remove(&key);
                    });
                }
                _ => {}
            }
        }
        self.forget_tab_lru(id);
        self.forget_tab_scroll_state(id);
        self.center_zone.update(|cz| cz.close(id));
    }

    pub fn activate_tab(&self, id: TabId) {
        // active_agent derives from center_zone via a Memo, so the tab change
        // here propagates without a manual sync.
        self.center_zone.update(|cz| cz.activate(id));
    }

    pub fn close_other_tabs(&self, id: TabId) {
        let exists = self
            .center_zone
            .with_untracked(|cz| cz.tabs.iter().any(|t| t.id == id));
        if !exists {
            return;
        }
        let to_close: Vec<_> = self.center_zone.with_untracked(|cz| {
            cz.tabs
                .iter()
                .filter(|t| t.id != id && t.closeable)
                .map(|t| (t.id, t.content.clone()))
                .collect()
        });
        for (tab_id, content) in &to_close {
            match content {
                TabContent::File { path } => {
                    let path = path.clone();
                    self.open_files.update(|files| {
                        files.remove(&path);
                    });
                }
                TabContent::Diff {
                    host_id,
                    project_id,
                    root,
                    scope,
                    path,
                } => {
                    let key = DiffKey::new(
                        host_id.clone(),
                        project_id.clone(),
                        root.clone(),
                        *scope,
                        path.clone(),
                    );
                    self.diff_contents.update(|diffs| {
                        diffs.remove(&key);
                    });
                }
                _ => {}
            }
            self.forget_tab_scroll_state(*tab_id);
        }
        self.center_zone.update(|cz| cz.close_others(id));
        self.prune_tab_lru();
    }

    pub fn close_tabs_to_right(&self, id: TabId) {
        let exists = self
            .center_zone
            .with_untracked(|cz| cz.tabs.iter().any(|t| t.id == id));
        if !exists {
            return;
        }
        let to_close: Vec<_> = self.center_zone.with_untracked(|cz| {
            let Some(idx) = cz.tabs.iter().position(|t| t.id == id) else {
                return vec![];
            };
            cz.tabs[idx + 1..]
                .iter()
                .filter(|t| t.closeable)
                .map(|t| (t.id, t.content.clone()))
                .collect()
        });
        for (tab_id, content) in &to_close {
            match content {
                TabContent::File { path } => {
                    let path = path.clone();
                    self.open_files.update(|files| {
                        files.remove(&path);
                    });
                }
                TabContent::Diff {
                    host_id,
                    project_id,
                    root,
                    scope,
                    path,
                } => {
                    let key = DiffKey::new(
                        host_id.clone(),
                        project_id.clone(),
                        root.clone(),
                        *scope,
                        path.clone(),
                    );
                    self.diff_contents.update(|diffs| {
                        diffs.remove(&key);
                    });
                }
                _ => {}
            }
            self.forget_tab_scroll_state(*tab_id);
        }
        self.center_zone.update(|cz| cz.close_to_right(id));
        self.prune_tab_lru();
    }

    pub fn close_all_tabs(&self) {
        let to_close: Vec<_> = self.center_zone.with_untracked(|cz| {
            cz.tabs
                .iter()
                .filter(|t| t.closeable)
                .map(|t| (t.id, t.content.clone()))
                .collect()
        });
        for (tab_id, content) in &to_close {
            match content {
                TabContent::File { path } => {
                    let path = path.clone();
                    self.open_files.update(|files| {
                        files.remove(&path);
                    });
                }
                TabContent::Diff {
                    host_id,
                    project_id,
                    root,
                    scope,
                    path,
                } => {
                    let key = DiffKey::new(
                        host_id.clone(),
                        project_id.clone(),
                        root.clone(),
                        *scope,
                        path.clone(),
                    );
                    self.diff_contents.update(|diffs| {
                        diffs.remove(&key);
                    });
                }
                _ => {}
            }
            self.forget_tab_scroll_state(*tab_id);
        }
        self.center_zone.update(|cz| cz.close_all());
        self.prune_tab_lru();
    }

    pub fn rename_tab_label(&self, id: TabId, new_label: String) {
        self.center_zone
            .update(|cz| cz.rename_tab_label(id, new_label));
    }

    /// If the currently active tab is a draft team-member chat tab (the user
    /// clicked a team member whose live agent hasn't been spawned yet),
    /// return the `(host_id, member_id)` pair. Used by the chat input send
    /// path to send `TeamMemberActivate { prompt: Some(_) }` instead of the
    /// normal `SpawnAgent::New`.
    pub fn active_pending_team_member_untracked(&self) -> Option<PendingTeamMember> {
        self.center_zone.with_untracked(|cz| {
            cz.active_tab().and_then(|tab| match &tab.content {
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
    fn close_others_keeps_target_and_non_closeable() {
        let home = make_tab(TabContent::Home, "Home", false);
        let chat1 = make_tab(TabContent::empty_chat(), "Chat 1", true);
        let chat2 = make_tab(TabContent::empty_chat(), "Chat 2", true);
        let target_id = chat1.id;
        let mut cz = CenterZoneState {
            tabs: vec![home, chat1, chat2],
            active_tab_id: None,
        };
        cz.close_others(target_id);
        assert_eq!(cz.tabs.len(), 2);
        assert!(cz.tabs.iter().any(|t| t.id == target_id));
        assert!(cz.tabs.iter().any(|t| !t.closeable));
        assert_eq!(cz.active_tab_id, Some(target_id));
    }

    #[test]
    fn close_to_right_removes_closeable_tabs_after_target() {
        let home = make_tab(TabContent::Home, "Home", false);
        let chat1 = make_tab(TabContent::empty_chat(), "Chat 1", true);
        let chat2 = make_tab(TabContent::empty_chat(), "Chat 2", true);
        let chat3 = make_tab(TabContent::empty_chat(), "Chat 3", true);
        let target_id = chat1.id;
        let mut cz = CenterZoneState {
            tabs: vec![home, chat1, chat2, chat3],
            active_tab_id: Some(target_id),
        };
        cz.close_to_right(target_id);
        assert_eq!(cz.tabs.len(), 2);
        assert!(cz.tabs.iter().any(|t| !t.closeable));
        assert!(cz.tabs.iter().any(|t| t.id == target_id));
        assert_eq!(cz.active_tab_id, Some(target_id));
    }

    #[test]
    fn close_all_keeps_only_non_closeable() {
        let home = make_tab(TabContent::Home, "Home", false);
        let home_id = home.id;
        let chat1 = make_tab(TabContent::empty_chat(), "Chat 1", true);
        let chat2 = make_tab(TabContent::empty_chat(), "Chat 2", true);
        let mut cz = CenterZoneState {
            tabs: vec![home, chat1, chat2],
            active_tab_id: None,
        };
        cz.close_all();
        assert_eq!(cz.tabs.len(), 1);
        assert!(matches!(cz.tabs[0].content, TabContent::Home));
        assert_eq!(cz.active_tab_id, Some(home_id));
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
            .with_untracked(|cz| cz.active_tab_id)
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
            tabs: vec![home, chat],
            active_tab_id: None,
        };
        cz.rename_tab_label(target_id, "New Name".to_string());
        assert_eq!(cz.tabs[0].label, "Home");
        assert_eq!(cz.tabs[1].label, "New Name");
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

    #[test]
    fn close_other_tabs_cleans_backing_state() {
        let owner = leptos::reactive::owner::Owner::new();
        owner.with(|| {
            let state = AppState::new();

            // Open a File tab and a Diff tab, keep Chat as the target
            let file_path = test_path("file_a");
            state.center_zone.update(|cz| {
                cz.open(
                    TabContent::File {
                        path: file_path.clone(),
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
                .with_untracked(|cz| cz.active_tab_id.unwrap());
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
                    file_path.clone(),
                    OpenFile {
                        path: file_path.clone(),
                        contents: None,
                        is_binary: false,
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
                    .with_untracked(|m| m.contains_key(&file_path))
            );
            assert!(
                !state
                    .diff_contents
                    .with_untracked(|m| m.contains_key(&diff_key))
            );
            state.center_zone.with_untracked(|cz| {
                assert_eq!(cz.tabs.len(), 2);
                assert!(cz.tabs.iter().any(|t| t.id == target_id));
                assert!(cz.tabs.iter().any(|t| !t.closeable));
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
                    .tabs
                    .iter()
                    .filter(|t| matches!(&t.content, TabContent::Diff { .. }))
                    .map(|t| t.label.as_str())
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
                .with_untracked(|cz| cz.active_tab_id.unwrap());
            let file_path = test_path("file_b");
            state.center_zone.update(|cz| {
                cz.open(
                    TabContent::File {
                        path: file_path.clone(),
                    },
                    "file_b.txt".to_string(),
                    true,
                );
            });
            state.open_files.update(|m| {
                m.insert(
                    file_path.clone(),
                    OpenFile {
                        path: file_path.clone(),
                        contents: None,
                        is_binary: false,
                    },
                );
            });

            state.close_tabs_to_right(target_id);

            assert!(
                !state
                    .open_files
                    .with_untracked(|m| m.contains_key(&file_path))
            );
            state.center_zone.with_untracked(|cz| {
                assert!(cz.tabs.iter().any(|t| t.id == target_id));
                assert!(!cz.tabs.iter().any(|t| {
                    matches!(&t.content, TabContent::File { path } if *path == file_path)
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
            state.center_zone.update(|cz| {
                cz.open(
                    TabContent::File {
                        path: file_path.clone(),
                    },
                    "file_c.txt".to_string(),
                    true,
                );
            });
            state.open_files.update(|m| {
                m.insert(
                    file_path.clone(),
                    OpenFile {
                        path: file_path.clone(),
                        contents: None,
                        is_binary: false,
                    },
                );
            });

            let tab_count_before = state.center_zone.with_untracked(|cz| cz.tabs.len());
            state.close_other_tabs(TabId(999_999));

            assert_eq!(
                state.center_zone.with_untracked(|cz| cz.tabs.len()),
                tab_count_before
            );
            assert!(
                state
                    .open_files
                    .with_untracked(|m| m.contains_key(&file_path))
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
                .with_untracked(|cz| cz.active_tab_id.expect("A tab active"));

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
                .with_untracked(|cz| cz.active_tab_id.expect("B tab active"));
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
    fn close_tabs_to_right_invalid_id_is_noop() {
        let owner = leptos::reactive::owner::Owner::new();
        owner.with(|| {
            let state = AppState::new();
            let file_path = test_path("file_d");
            state.center_zone.update(|cz| {
                cz.open(
                    TabContent::File {
                        path: file_path.clone(),
                    },
                    "file_d.txt".to_string(),
                    true,
                );
            });
            state.open_files.update(|m| {
                m.insert(
                    file_path.clone(),
                    OpenFile {
                        path: file_path.clone(),
                        contents: None,
                        is_binary: false,
                    },
                );
            });

            let tab_count_before = state.center_zone.with_untracked(|cz| cz.tabs.len());
            state.close_tabs_to_right(TabId(999_998));

            assert_eq!(
                state.center_zone.with_untracked(|cz| cz.tabs.len()),
                tab_count_before
            );
            assert!(
                state
                    .open_files
                    .with_untracked(|m| m.contains_key(&file_path))
            );
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
                token_usage: Some(protocol::TokenUsage {
                    input_tokens: 7,
                    output_tokens: 3,
                    total_tokens: 10,
                    cached_prompt_tokens: None,
                    cache_creation_input_tokens: None,
                    reasoning_tokens: None,
                }),
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
                    .is_some_and(|t| t.total_tokens == 10),
                "token_usage patched"
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
                    .is_some_and(|t| t.total_tokens == 10),
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
}
