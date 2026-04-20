use std::cell::Cell;
use std::collections::HashMap;

use crate::bridge::ConfiguredHost;
use leptos::prelude::*;
use protocol::{
    AgentId, AgentOrigin, BackendKind, BackendSetupInfo, ChatMessage, CustomAgent, CustomAgentId,
    HostAbsPath, HostBrowseEntry, HostBrowseErrorPayload, HostPlatform, HostSettings,
    McpServerConfig, McpServerId, Project, ProjectDiffScope, ProjectFileEntry, ProjectGitDiffFile,
    ProjectId, ProjectPath, ProjectRootGitStatus, ProjectRootPath, QueuedMessageEntry,
    SessionSchemaEntry, SessionSettingsValues, SessionSummary, Skill, SkillId, Steering,
    SteeringId, StreamPath, TaskList, TerminalId, ToolExecutionCompletedData, ToolRequest,
};

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
    static NEXT_TAB_ID: Cell<u64> = Cell::new(0);
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
        if let Some(active_id) = self.active_tab_id {
            if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == active_id) {
                tab.content = content;
                tab.label = label;
                tab.closeable = closeable;
                return active_id;
            }
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
    pub files: Vec<ProjectGitDiffFile>,
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
    pub file_tree: RwSignal<HashMap<ProjectId, Vec<ProjectFileEntry>>>,
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
    pub custom_agents: RwSignal<HashMap<String, HashMap<CustomAgentId, CustomAgent>>>,
    pub mcp_servers: RwSignal<HashMap<String, HashMap<McpServerId, McpServerConfig>>>,
    pub steering: RwSignal<HashMap<String, HashMap<SteeringId, Steering>>>,
    pub skills: RwSignal<HashMap<String, HashMap<SkillId, Skill>>>,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            configured_hosts: RwSignal::new(Vec::new()),
            selected_host_id: RwSignal::new(None),
            host_streams: RwSignal::new(HashMap::new()),
            connection_statuses: RwSignal::new(HashMap::new()),
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
            custom_agents: RwSignal::new(HashMap::new()),
            mcp_servers: RwSignal::new(HashMap::new()),
            steering: RwSignal::new(HashMap::new()),
            skills: RwSignal::new(HashMap::new()),
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
        self.host_streams.update(|streams| {
            streams.remove(host_id);
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
}
