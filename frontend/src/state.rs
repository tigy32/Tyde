use std::cell::Cell;
use std::collections::HashMap;

use crate::bridge::{ConfiguredHost, RemoteHostLifecycleStatus};
use leptos::prelude::*;
use protocol::{
    AgentId, AgentOrigin, BackendKind, BackendSetupInfo, ChatMessage, CustomAgent, CustomAgentId,
    DiffContextMode, HostAbsPath, HostBrowseEntry, HostBrowseErrorPayload, HostPlatform,
    HostSettings, McpServerConfig, McpServerId, Project, ProjectDiffScope, ProjectGitDiffFile,
    ProjectGitDiffPayload, ProjectId, ProjectPath, ProjectRootGitStatus, ProjectRootListing,
    ProjectRootPath, QueuedMessageEntry, SessionSchemaEntry, SessionSettingsValues, SessionSummary,
    Skill, SkillId, Steering, SteeringId, StreamPath, TaskList, TerminalId,
    ToolExecutionCompletedData, ToolRequest,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffViewMode {
    Unified,
    SideBySide,
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
    pub custom_agent_id: Option<CustomAgentId>,
    pub created_at_ms: u64,
    pub instance_stream: StreamPath,
    pub started: bool,
    /// Set when a fatal `AgentError` arrives. The agent is terminated and no
    /// further events will arrive on its stream.
    pub fatal_error: Option<String>,
}

// ── Tab system ──────────────────────────────────────────────────────────

thread_local! {
    static NEXT_TAB_ID: Cell<u64> = const { Cell::new(0) };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TabId(pub u64);

pub fn next_tab_id() -> TabId {
    NEXT_TAB_ID.with(|cell| {
        let id = cell.get();
        cell.set(id + 1);
        TabId(id)
    })
}

#[derive(Clone, Debug, PartialEq)]
pub enum TabContent {
    Home,
    Chat {
        agent_ref: Option<ActiveAgentRef>,
    },
    File {
        path: ProjectPath,
    },
    Diff {
        root: ProjectRootPath,
        scope: ProjectDiffScope,
    },
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
pub struct OpenFile {
    pub path: ProjectPath,
    pub contents: Option<String>,
    pub is_binary: bool,
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
    pub tool_requests: ArcRwSignal<Vec<ToolRequestEntry>>,
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
    pub host_id: String,
    pub browse_stream: StreamPath,
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
    pub active_agent: Option<ActiveAgentRef>,
    pub active_terminal: Option<ActiveTerminalRef>,
    pub open_files: HashMap<ProjectPath, OpenFile>,
    pub diff_contents: HashMap<(ProjectRootPath, ProjectDiffScope), DiffViewState>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ActiveProjectRef {
    pub host_id: String,
    pub project_id: ProjectId,
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
    pub active_agent: RwSignal<Option<ActiveAgentRef>>,
    pub chat_messages: RwSignal<HashMap<AgentId, Vec<ChatMessageEntry>>>,
    pub streaming_text: RwSignal<HashMap<AgentId, StreamingState>>,
    pub chat_input: RwSignal<String>,
    pub task_lists: RwSignal<HashMap<AgentId, TaskList>>,
    pub center_zone: RwSignal<CenterZoneState>,
    pub tabs_enabled: RwSignal<bool>,
    pub left_dock: RwSignal<DockVisibility>,
    pub right_dock: RwSignal<DockVisibility>,
    pub bottom_dock: RwSignal<DockVisibility>,
    pub file_tree: RwSignal<HashMap<ProjectId, Vec<ProjectRootListing>>>,
    pub git_status: RwSignal<HashMap<ProjectId, Vec<ProjectRootGitStatus>>>,
    pub open_files: RwSignal<HashMap<ProjectPath, OpenFile>>,
    pub diff_contents: RwSignal<HashMap<(ProjectRootPath, ProjectDiffScope), DiffViewState>>,
    pub terminals: RwSignal<Vec<TerminalInfo>>,
    pub active_terminal: RwSignal<Option<ActiveTerminalRef>>,
    pub transient_events: RwSignal<HashMap<AgentId, Vec<TransientEvent>>>,
    pub browse_dialog: RwSignal<Option<BrowseDialogState>>,
    /// Per-project snapshots of center-zone state. Updated whenever the user
    /// switches away from a project; consulted on switch-in to restore.
    pub project_view_memory: RwSignal<HashMap<ActiveProjectRef, ProjectViewMemory>>,
    pub command_palette_open: RwSignal<bool>,
    pub settings_open: RwSignal<bool>,
    pub feedback_open: RwSignal<bool>,
    pub find_bar_open: RwSignal<bool>,
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
    pub diff_view_mode: RwSignal<DiffViewMode>,
    pub diff_context_mode: RwSignal<DiffContextMode>,
    pub custom_agents: RwSignal<HashMap<String, HashMap<CustomAgentId, CustomAgent>>>,
    pub mcp_servers: RwSignal<HashMap<String, HashMap<McpServerId, McpServerConfig>>>,
    pub steering: RwSignal<HashMap<String, HashMap<SteeringId, Steering>>>,
    pub skills: RwSignal<HashMap<String, HashMap<SkillId, Skill>>>,
    pub agents_panel_filters: RwSignal<HashMap<Option<ActiveProjectRef>, AgentsPanelFilters>>,
}

impl AppState {
    pub fn new() -> Self {
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
            active_agent: RwSignal::new(None),
            chat_messages: RwSignal::new(HashMap::new()),
            streaming_text: RwSignal::new(HashMap::new()),
            chat_input: RwSignal::new(String::new()),
            task_lists: RwSignal::new(HashMap::new()),
            center_zone: RwSignal::new(CenterZoneState::default()),
            tabs_enabled: RwSignal::new(true),
            left_dock: RwSignal::new(DockVisibility::Visible),
            right_dock: RwSignal::new(DockVisibility::Visible),
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
            feedback_open: RwSignal::new(false),
            find_bar_open: RwSignal::new(false),
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
            diff_view_mode: RwSignal::new(DiffViewMode::Unified),
            diff_context_mode: RwSignal::new(DiffContextMode::Hunks),
            custom_agents: RwSignal::new(HashMap::new()),
            mcp_servers: RwSignal::new(HashMap::new()),
            steering: RwSignal::new(HashMap::new()),
            skills: RwSignal::new(HashMap::new()),
            agents_panel_filters: RwSignal::new(HashMap::new()),
        }
    }

    pub fn selected_host(&self) -> Option<ConfiguredHost> {
        let selected = self.selected_host_id.get()?;
        self.configured_hosts
            .get()
            .into_iter()
            .find(|host| host.id == selected)
    }

    pub fn host_stream_untracked(&self, host_id: &str) -> Option<StreamPath> {
        self.host_streams.get_untracked().get(host_id).cloned()
    }

    pub fn selected_host_stream_untracked(&self) -> Option<(String, StreamPath)> {
        let host_id = self.selected_host_id.get_untracked()?;
        let stream = self.host_stream_untracked(&host_id)?;
        Some((host_id, stream))
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

    pub fn active_project_ref_untracked(&self) -> Option<ActiveProjectRef> {
        self.active_project.get_untracked()
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
                active_agent: self.active_agent.get_untracked(),
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

        match (next.is_some(), restored) {
            (true, Some(memory)) => {
                self.center_zone.set(memory.center_zone.unwrap_or_default());
                self.active_agent.set(memory.active_agent);
                self.active_terminal.set(memory.active_terminal);
                self.open_files.set(memory.open_files);
                self.diff_contents.set(memory.diff_contents);
            }
            (true, None) => {
                let mut cz = CenterZoneState::default();
                cz.open(
                    TabContent::Chat { agent_ref: None },
                    "New Chat".to_string(),
                    true,
                );
                self.center_zone.set(cz);
                self.active_agent.set(None);
                self.active_terminal.set(None);
                self.open_files.set(HashMap::new());
                self.diff_contents.set(HashMap::new());
            }
            (false, _) => {
                self.center_zone.set(CenterZoneState::default());
                self.active_agent.set(None);
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
            let drop_set: std::collections::HashSet<AgentId> =
                agent_ids.iter().cloned().collect();
            self.chat_messages.update(|map| {
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

        if self
            .active_project
            .get_untracked()
            .as_ref()
            .is_some_and(|active| active.host_id == host_id)
        {
            self.switch_active_project(None);
        }
        if self
            .active_agent
            .get_untracked()
            .as_ref()
            .is_some_and(|active| active.host_id == host_id)
        {
            self.active_agent.set(None);
        }
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
        self.center_zone.update(|cz| {
            if tabs_enabled {
                cz.open(content, label, closeable);
            } else {
                cz.replace_active(content, label, closeable);
            }
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
                TabContent::Diff { root, scope } => {
                    let key = (root.clone(), *scope);
                    self.diff_contents.update(|diffs| {
                        diffs.remove(&key);
                    });
                }
                _ => {}
            }
        }
        self.center_zone.update(|cz| cz.close(id));
    }

    pub fn activate_tab(&self, id: TabId) {
        self.center_zone.update(|cz| cz.activate(id));
        // Sync active_agent when switching to a chat tab
        let agent_ref = self.center_zone.with_untracked(|cz| {
            cz.active_tab().and_then(|tab| match &tab.content {
                TabContent::Chat { agent_ref } => Some(agent_ref.clone()),
                _ => None,
            })
        });
        if let Some(ar) = agent_ref {
            self.active_agent.set(ar);
        }
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
                .map(|t| t.content.clone())
                .collect()
        });
        for content in &to_close {
            match content {
                TabContent::File { path } => {
                    let path = path.clone();
                    self.open_files.update(|files| {
                        files.remove(&path);
                    });
                }
                TabContent::Diff { root, scope } => {
                    let key = (root.clone(), *scope);
                    self.diff_contents.update(|diffs| {
                        diffs.remove(&key);
                    });
                }
                _ => {}
            }
        }
        self.center_zone.update(|cz| cz.close_others(id));
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
                .map(|t| t.content.clone())
                .collect()
        });
        for content in &to_close {
            match content {
                TabContent::File { path } => {
                    let path = path.clone();
                    self.open_files.update(|files| {
                        files.remove(&path);
                    });
                }
                TabContent::Diff { root, scope } => {
                    let key = (root.clone(), *scope);
                    self.diff_contents.update(|diffs| {
                        diffs.remove(&key);
                    });
                }
                _ => {}
            }
        }
        self.center_zone.update(|cz| cz.close_to_right(id));
    }

    pub fn close_all_tabs(&self) {
        let to_close: Vec<_> = self.center_zone.with_untracked(|cz| {
            cz.tabs
                .iter()
                .filter(|t| t.closeable)
                .map(|t| t.content.clone())
                .collect()
        });
        for content in &to_close {
            match content {
                TabContent::File { path } => {
                    let path = path.clone();
                    self.open_files.update(|files| {
                        files.remove(&path);
                    });
                }
                TabContent::Diff { root, scope } => {
                    let key = (root.clone(), *scope);
                    self.diff_contents.update(|diffs| {
                        diffs.remove(&key);
                    });
                }
                _ => {}
            }
        }
        self.center_zone.update(|cz| cz.close_all());
    }

    pub fn rename_tab_label(&self, id: TabId, new_label: String) {
        self.center_zone
            .update(|cz| cz.rename_tab_label(id, new_label));
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
        let chat1 = make_tab(TabContent::Chat { agent_ref: None }, "Chat 1", true);
        let chat2 = make_tab(TabContent::Chat { agent_ref: None }, "Chat 2", true);
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
        let chat1 = make_tab(TabContent::Chat { agent_ref: None }, "Chat 1", true);
        let chat2 = make_tab(TabContent::Chat { agent_ref: None }, "Chat 2", true);
        let chat3 = make_tab(TabContent::Chat { agent_ref: None }, "Chat 3", true);
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
        let chat1 = make_tab(TabContent::Chat { agent_ref: None }, "Chat 1", true);
        let chat2 = make_tab(TabContent::Chat { agent_ref: None }, "Chat 2", true);
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
    fn rename_tab_label_only_changes_target() {
        let home = make_tab(TabContent::Home, "Home", false);
        let chat = make_tab(TabContent::Chat { agent_ref: None }, "Old Name", true);
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

    fn test_diff_state(root: ProjectRootPath, scope: ProjectDiffScope) -> DiffViewState {
        DiffViewState {
            root: root.clone(),
            scope,
            path: None,
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
                cz.open(
                    TabContent::Chat { agent_ref: None },
                    "Chat".to_string(),
                    true,
                );
            });
            let target_id = state
                .center_zone
                .with_untracked(|cz| cz.active_tab_id.unwrap());
            let diff_root = ProjectRootPath("/root/proj".to_string());
            let diff_scope = ProjectDiffScope::Unstaged;
            state.center_zone.update(|cz| {
                cz.open(
                    TabContent::Diff {
                        root: diff_root.clone(),
                        scope: diff_scope,
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
            state.diff_contents.update(|m| {
                m.insert(
                    (diff_root.clone(), diff_scope),
                    test_diff_state(diff_root.clone(), diff_scope),
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
                    .with_untracked(|m| m.contains_key(&(diff_root, diff_scope)))
            );
            state.center_zone.with_untracked(|cz| {
                assert_eq!(cz.tabs.len(), 2);
                assert!(cz.tabs.iter().any(|t| t.id == target_id));
                assert!(cz.tabs.iter().any(|t| !t.closeable));
            });
        });
    }

    #[test]
    fn close_tabs_to_right_cleans_backing_state() {
        let owner = leptos::reactive::owner::Owner::new();
        owner.with(|| {
            let state = AppState::new();

            state.center_zone.update(|cz| {
                cz.open(
                    TabContent::Chat { agent_ref: None },
                    "Chat".to_string(),
                    true,
                );
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
                custom_agent_id: None,
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
                state.chat_messages.update(|m| {
                    m.insert(id.clone(), vec![mk_msg()]);
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
                    !state
                        .chat_messages
                        .with_untracked(|m| m.contains_key(id)),
                    "chat_messages still has dropped agent {}",
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
                    .chat_messages
                    .with_untracked(|m| m.contains_key(&agent_b1)),
                "host_b agent's chat_messages must survive"
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
}
