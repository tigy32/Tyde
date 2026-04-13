use std::collections::HashMap;

use leptos::prelude::*;
use protocol::{
    AgentId, BackendKind, ChatMessage, ProjectDiffScope, ProjectFileEntry, ProjectGitDiffFile,
    ProjectId, ProjectPath, ProjectRootGitStatus, ProjectRootPath, Project, SessionSummary,
    StreamPath, TaskList, TerminalId, ToolExecutionCompletedData, ToolRequest,
};

#[derive(Clone, Debug, PartialEq)]
pub enum ConnectionStatus {
    Disconnected,
    Connecting,
    Connected,
    Error(String),
}

#[derive(Clone, Debug, PartialEq)]
pub enum AgentStatus {
    Starting,
    Running,
    Completed,
    Error(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct AgentInfo {
    pub agent_id: AgentId,
    pub name: String,
    pub backend_kind: BackendKind,
    pub workspace_roots: Vec<String>,
    pub project_id: Option<ProjectId>,
    pub parent_agent_id: Option<AgentId>,
    pub created_at_ms: u64,
    pub instance_stream: StreamPath,
    pub status: AgentStatus,
}

#[derive(Clone, Debug, PartialEq)]
pub enum CenterView {
    Home,
    Chat,
    Editor,
}

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
    pub text: String,
    pub reasoning: String,
}

#[derive(Clone, Debug)]
pub struct TerminalInfo {
    pub terminal_id: TerminalId,
    pub stream: StreamPath,
    pub project_id: Option<ProjectId>,
    pub cwd: String,
    pub shell: String,
    pub cols: u16,
    pub rows: u16,
    pub created_at_ms: u64,
    pub output_buffer: String,
    pub exited: bool,
    pub exit_code: Option<i32>,
}

#[derive(Clone, Debug)]
pub enum TransientEvent {
    OperationCancelled { message: String },
    RetryAttempt { attempt: u64, max_retries: u64, error: String, backoff_ms: u64 },
}

#[derive(Clone)]
pub struct AppState {
    pub host_id: RwSignal<Option<String>>,
    pub host_stream: RwSignal<Option<StreamPath>>,
    pub connection_status: RwSignal<ConnectionStatus>,
    pub projects: RwSignal<Vec<Project>>,
    pub agents: RwSignal<Vec<AgentInfo>>,
    pub sessions: RwSignal<Vec<SessionSummary>>,
    pub active_project_id: RwSignal<Option<ProjectId>>,
    pub active_agent_id: RwSignal<Option<AgentId>>,
    pub chat_messages: RwSignal<HashMap<AgentId, Vec<ChatMessageEntry>>>,
    pub streaming_text: RwSignal<HashMap<AgentId, StreamingState>>,
    pub chat_input: RwSignal<String>,
    pub task_lists: RwSignal<HashMap<AgentId, TaskList>>,
    pub center_view: RwSignal<CenterView>,
    pub left_dock: RwSignal<DockVisibility>,
    pub right_dock: RwSignal<DockVisibility>,
    pub bottom_dock: RwSignal<DockVisibility>,
    pub file_tree: RwSignal<HashMap<ProjectId, Vec<ProjectFileEntry>>>,
    pub git_status: RwSignal<HashMap<ProjectId, Vec<ProjectRootGitStatus>>>,
    pub open_file: RwSignal<Option<OpenFile>>,
    pub diff_content: RwSignal<Option<DiffViewState>>,
    pub terminals: RwSignal<Vec<TerminalInfo>>,
    pub active_terminal_id: RwSignal<Option<TerminalId>>,
    pub transient_events: RwSignal<HashMap<AgentId, Vec<TransientEvent>>>,
    pub adding_project: RwSignal<bool>,
    pub command_palette_open: RwSignal<bool>,
    pub settings_open: RwSignal<bool>,
    pub default_backend: RwSignal<BackendKind>,
    pub font_size: RwSignal<u32>,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            host_id: RwSignal::new(None),
            host_stream: RwSignal::new(None),
            connection_status: RwSignal::new(ConnectionStatus::Disconnected),
            projects: RwSignal::new(Vec::new()),
            agents: RwSignal::new(Vec::new()),
            sessions: RwSignal::new(Vec::new()),
            active_project_id: RwSignal::new(None),
            active_agent_id: RwSignal::new(None),
            chat_messages: RwSignal::new(HashMap::new()),
            streaming_text: RwSignal::new(HashMap::new()),
            chat_input: RwSignal::new(String::new()),
            task_lists: RwSignal::new(HashMap::new()),
            center_view: RwSignal::new(CenterView::Home),
            left_dock: RwSignal::new(DockVisibility::Visible),
            right_dock: RwSignal::new(DockVisibility::Hidden),
            bottom_dock: RwSignal::new(DockVisibility::Hidden),
            file_tree: RwSignal::new(HashMap::new()),
            git_status: RwSignal::new(HashMap::new()),
            open_file: RwSignal::new(None),
            diff_content: RwSignal::new(None),
            terminals: RwSignal::new(Vec::new()),
            active_terminal_id: RwSignal::new(None),
            transient_events: RwSignal::new(HashMap::new()),
            adding_project: RwSignal::new(false),
            command_palette_open: RwSignal::new(false),
            settings_open: RwSignal::new(false),
            default_backend: RwSignal::new(BackendKind::Claude),
            font_size: RwSignal::new(14),
        }
    }
}
