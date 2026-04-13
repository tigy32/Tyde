use std::future::Future;
use std::pin::Pin;

use serde::de::DeserializeOwned;
use serde_json::Value;
use tyde_protocol::protocol as proto;

use crate::backends::tycode::ImageAttachment;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Debug, Clone)]
pub enum InvokeRequest {
    Conversation(ConversationInvoke),
    Agent(AgentInvoke),
    Definitions(DefinitionsInvoke),
    Git(GitInvoke),
    File(FileInvoke),
    Terminal(TerminalInvoke),
    Admin(AdminInvoke),
    Backend(BackendInvoke),
    SessionRecord(SessionRecordInvoke),
    Project(ProjectInvoke),
    System(SystemInvoke),
}

#[derive(Debug, Clone)]
pub enum ConversationInvoke {
    SendMessage {
        agent_id: String,
        message: String,
        images: Option<Vec<ImageAttachment>>,
    },
    CancelAgent {
        agent_id: String,
    },
    CloseAgent {
        agent_id: String,
    },
    SessionPassthrough {
        agent_id: String,
        command: SessionPassthroughCommand,
    },
    ResumeSession {
        agent_id: String,
        session_id: String,
    },
    DeleteSession {
        agent_id: String,
        session_id: String,
    },
    SwitchProfile {
        agent_id: String,
        profile_name: String,
    },
    UpdateSettings {
        agent_id: String,
        settings: proto::SessionSettingsData,
        persist: bool,
    },
    RestartSubprocess {
        agent_id: String,
    },
    RelaunchAgent {
        agent_id: String,
    },
}

#[derive(Debug, Clone, Copy)]
pub enum SessionPassthroughCommand {
    GetSettings,
    ListSessions,
    GetModuleSchemas,
    ListModels,
    ListProfiles,
}

#[derive(Debug, Clone)]
pub enum AgentInvoke {
    CreateAgent {
        workspace_roots: Vec<String>,
        backend_kind: Option<String>,
        ephemeral: Option<bool>,
        agent_definition_id: Option<String>,
        ui_owner_project_id: Option<String>,
    },
    SpawnAgent {
        workspace_roots: Vec<String>,
        prompt: String,
        backend_kind: Option<String>,
        parent_agent_id: Option<String>,
        ui_owner_project_id: Option<String>,
        name: String,
        ephemeral: Option<bool>,
        agent_definition_id: Option<String>,
    },
    SendAgentMessage {
        agent_id: String,
        message: String,
    },
    InterruptAgent {
        agent_id: String,
    },
    TerminateAgent {
        agent_id: String,
    },
    CancelAgent {
        agent_id: String,
    },
    ListAgents,
    WaitForAgent {
        agent_id: String,
    },
    CollectAgentResult {
        agent_id: String,
    },
    GetAgent {
        agent_id: String,
    },
    AgentEventsSince {
        since_seq: Option<u64>,
        limit: Option<usize>,
    },
    RenameAgent {
        agent_id: String,
        name: String,
    },
}

#[derive(Debug, Clone)]
pub enum DefinitionsInvoke {
    ListAgentDefinitions {
        workspace_path: Option<String>,
    },
    SaveAgentDefinition {
        definition_json: String,
        scope: String,
        workspace_path: Option<String>,
    },
    DeleteAgentDefinition {
        id: String,
        scope: String,
        workspace_path: Option<String>,
    },
    ListAvailableSkills {
        workspace_path: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub enum GitInvoke {
    DiscoverRepos {
        workspace_dir: String,
    },
    CurrentBranch {
        working_dir: String,
    },
    Status {
        working_dir: String,
    },
    Stage {
        working_dir: String,
        paths: Vec<String>,
    },
    Unstage {
        working_dir: String,
        paths: Vec<String>,
    },
    Commit {
        working_dir: String,
        message: String,
    },
    Diff {
        working_dir: String,
        path: String,
        staged: bool,
    },
    DiffBaseContent {
        working_dir: String,
        path: String,
        staged: bool,
    },
    Discard {
        working_dir: String,
        paths: Vec<String>,
    },
    WorktreeAdd {
        working_dir: String,
        path: String,
        branch: String,
    },
    WorktreeRemove {
        working_dir: String,
        path: String,
    },
}

#[derive(Debug, Clone)]
pub enum FileInvoke {
    ListDirectory {
        path: String,
        show_hidden: Option<bool>,
    },
    ReadFileContent {
        path: String,
    },
    SyncFileWatchPaths {
        paths: Vec<String>,
    },
    WatchWorkspaceDir {
        path: String,
    },
    UnwatchWorkspaceDir,
}

#[derive(Debug, Clone)]
pub enum TerminalInvoke {
    Create {
        workspace_path: String,
    },
    Write {
        terminal_id: u64,
        data: String,
    },
    Resize {
        terminal_id: u64,
        cols: u16,
        rows: u16,
    },
    Close {
        terminal_id: u64,
    },
}

#[derive(Debug, Clone)]
pub enum AdminInvoke {
    CreateSubprocess {
        workspace_roots: Vec<String>,
        backend_kind: Option<String>,
    },
    CloseSubprocess {
        admin_id: u64,
    },
    ListSessions {
        admin_id: u64,
    },
    GetSettings {
        admin_id: u64,
    },
    UpdateSettings {
        admin_id: u64,
        settings: proto::SessionSettingsData,
    },
    ListProfiles {
        admin_id: u64,
    },
    SwitchProfile {
        admin_id: u64,
        profile_name: String,
    },
    GetModuleSchemas {
        admin_id: u64,
    },
    DeleteSession {
        admin_id: u64,
        session_id: String,
    },
}

#[derive(Debug, Clone)]
pub enum BackendInvoke {
    QueryUsage {
        backend_kind: String,
        host_id: Option<String>,
    },
    CheckDependencies,
    InstallDependency {
        backend_kind: String,
    },
}

#[derive(Debug, Clone)]
pub enum SessionRecordInvoke {
    List {
        workspace_root: Option<String>,
        host_id: Option<String>,
    },
    GetSessionId {
        agent_id: String,
    },
    Rename {
        id: String,
        name: String,
        host_id: Option<String>,
    },
    SetAlias {
        id: String,
        alias: String,
        host_id: Option<String>,
    },
    Delete {
        id: String,
        host_id: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub enum ProjectInvoke {
    List {
        host: Option<String>,
    },
    Add {
        host: Option<String>,
        workspace_path: String,
        name: String,
    },
    AddWorkbench {
        host: Option<String>,
        parent_project_id: String,
        workspace_path: String,
        name: String,
        kind: String,
    },
    Remove {
        host: Option<String>,
        id: String,
    },
    RemoveByWorkspacePath {
        workspace_path: String,
    },
    Rename {
        host: Option<String>,
        id: String,
        name: String,
    },
    UpdateRoots {
        host: Option<String>,
        id: String,
        roots: Vec<String>,
    },
    RegisterGitWorkbench {
        parent_workspace_path: String,
        worktree_path: String,
        branch: String,
    },
}

#[derive(Debug, Clone)]
pub enum SystemInvoke {
    ServerStatus,
    SetDefaultBackend {
        backend: String,
    },
    SetDisabledBackends {
        backends: Vec<String>,
    },
    GetMcpHttpServerSettings,
    SetMcpHttpServerEnabled {
        enabled: bool,
    },
    GetDriverMcpHttpServerSettings,
    SetDriverMcpHttpServerEnabled {
        enabled: bool,
    },
    SetDriverMcpHttpServerAutoloadEnabled {
        enabled: bool,
    },
    SetMcpControlEnabled {
        enabled: bool,
    },
    GetRemoteControlSettings,
    SetRemoteControlEnabled {
        enabled: bool,
    },
    ListHosts,
    AddHost {
        label: String,
        hostname: String,
    },
    RemoveHost {
        id: String,
    },
    UpdateHostLabel {
        id: String,
        label: String,
    },
    UpdateHostEnabledBackends {
        id: String,
        backends: Vec<String>,
    },
    UpdateHostDefaultBackend {
        id: String,
        backend: String,
    },
    GetHostForWorkspace {
        workspace_path: String,
    },
    ListActiveAgents,
    ListWorkflows {
        workspace_path: Option<String>,
    },
    SaveWorkflow {
        workflow_json: String,
        scope: String,
        workspace_path: Option<String>,
    },
    DeleteWorkflow {
        id: String,
        scope: String,
        workspace_path: Option<String>,
    },
    RunShellCommand {
        command: String,
        cwd: String,
    },
}

impl InvokeRequest {
    pub fn parse(command: &str, params: Value) -> Result<Self, String> {
        match command {
            "create_agent" => {
                let p: proto::CreateAgentParams = parse_params(params)?;
                Ok(Self::Agent(AgentInvoke::CreateAgent {
                    workspace_roots: p.workspace_roots,
                    backend_kind: p.backend_kind.map(|kind| kind.as_str().to_string()),
                    ephemeral: p.ephemeral,
                    agent_definition_id: p.agent_definition_id,
                    ui_owner_project_id: p.ui_owner_project_id,
                }))
            }
            "send_message" => {
                let p: proto::SendMessageParams = parse_params(params)?;
                Ok(Self::Conversation(ConversationInvoke::SendMessage {
                    agent_id: p.conversation_id,
                    message: p.message,
                    images: p.images.map(|images| {
                        images
                            .into_iter()
                            .map(|image| ImageAttachment {
                                data: image.data,
                                media_type: image.media_type,
                                name: image.name,
                                size: image.size,
                            })
                            .collect()
                    }),
                }))
            }
            "cancel_conversation" => {
                let p: proto::ConversationIdParams = parse_params(params)?;
                Ok(Self::Conversation(ConversationInvoke::CancelAgent {
                    agent_id: p.conversation_id,
                }))
            }
            "close_conversation" => {
                let p: proto::ConversationIdParams = parse_params(params)?;
                Ok(Self::Conversation(ConversationInvoke::CloseAgent {
                    agent_id: p.conversation_id,
                }))
            }
            "get_settings" | "list_sessions" | "get_module_schemas" | "list_models"
            | "list_profiles" => {
                let p: proto::ConversationIdParams = parse_params(params)?;
                let command = match command {
                    "get_settings" => SessionPassthroughCommand::GetSettings,
                    "list_sessions" => SessionPassthroughCommand::ListSessions,
                    "get_module_schemas" => SessionPassthroughCommand::GetModuleSchemas,
                    "list_models" => SessionPassthroughCommand::ListModels,
                    "list_profiles" => SessionPassthroughCommand::ListProfiles,
                    _ => unreachable!(),
                };
                Ok(Self::Conversation(ConversationInvoke::SessionPassthrough {
                    agent_id: p.conversation_id,
                    command,
                }))
            }
            "resume_session" => {
                let p: proto::ConversationSessionParams = parse_params(params)?;
                Ok(Self::Conversation(ConversationInvoke::ResumeSession {
                    agent_id: p.conversation_id,
                    session_id: p.session_id,
                }))
            }
            "delete_session" => {
                let p: proto::ConversationSessionParams = parse_params(params)?;
                Ok(Self::Conversation(ConversationInvoke::DeleteSession {
                    agent_id: p.conversation_id,
                    session_id: p.session_id,
                }))
            }
            "switch_profile" => {
                let p: proto::ConversationProfileParams = parse_params(params)?;
                Ok(Self::Conversation(ConversationInvoke::SwitchProfile {
                    agent_id: p.conversation_id,
                    profile_name: p.profile_name,
                }))
            }
            "update_settings" => {
                let p: proto::UpdateSettingsParams = parse_params(params)?;
                Ok(Self::Conversation(ConversationInvoke::UpdateSettings {
                    agent_id: p.conversation_id,
                    settings: p.settings,
                    persist: p.persist.unwrap_or(false),
                }))
            }
            "restart_subprocess" => {
                let p: proto::ConversationIdParams = parse_params(params)?;
                Ok(Self::Conversation(ConversationInvoke::RestartSubprocess {
                    agent_id: p.conversation_id,
                }))
            }
            "relaunch_conversation" => {
                let p: proto::ConversationIdParams = parse_params(params)?;
                Ok(Self::Conversation(ConversationInvoke::RelaunchAgent {
                    agent_id: p.conversation_id,
                }))
            }
            "spawn_agent" => {
                let p: proto::SpawnAgentParams = parse_params(params)?;
                Ok(Self::Agent(AgentInvoke::SpawnAgent {
                    workspace_roots: p.workspace_roots,
                    prompt: p.prompt,
                    backend_kind: p.backend_kind.map(|kind| kind.as_str().to_string()),
                    parent_agent_id: p.parent_agent_id,
                    ui_owner_project_id: p.ui_owner_project_id,
                    name: p
                        .name
                        .map(|name| name.trim().to_string())
                        .filter(|name| !name.is_empty())
                        .unwrap_or_else(|| "Sub-agent".to_string()),
                    ephemeral: p.ephemeral,
                    agent_definition_id: p.agent_definition_id,
                }))
            }
            "send_agent_message" => {
                let p: proto::AgentIdMessageParams = parse_params(params)?;
                Ok(Self::Agent(AgentInvoke::SendAgentMessage {
                    agent_id: p.agent_id,
                    message: p.message,
                }))
            }
            "interrupt_agent" => {
                let p: proto::AgentIdParams = parse_params(params)?;
                Ok(Self::Agent(AgentInvoke::InterruptAgent {
                    agent_id: p.agent_id,
                }))
            }
            "terminate_agent" => {
                let p: proto::AgentIdParams = parse_params(params)?;
                Ok(Self::Agent(AgentInvoke::TerminateAgent {
                    agent_id: p.agent_id,
                }))
            }
            "cancel_agent" => {
                let p: proto::AgentIdParams = parse_params(params)?;
                Ok(Self::Agent(AgentInvoke::CancelAgent {
                    agent_id: p.agent_id,
                }))
            }
            "list_agents" => Ok(Self::Agent(AgentInvoke::ListAgents)),
            "wait_for_agent" => {
                let p: proto::WaitForAgentParams = parse_params(params)?;
                Ok(Self::Agent(AgentInvoke::WaitForAgent {
                    agent_id: p.agent_id,
                }))
            }
            "collect_agent_result" => {
                let p: proto::AgentIdParams = parse_params(params)?;
                Ok(Self::Agent(AgentInvoke::CollectAgentResult {
                    agent_id: p.agent_id,
                }))
            }
            "get_agent" => {
                let p: proto::AgentIdParams = parse_params(params)?;
                Ok(Self::Agent(AgentInvoke::GetAgent {
                    agent_id: p.agent_id,
                }))
            }
            "agent_events_since" => {
                let p: proto::AgentEventsSinceParams = parse_params(params)?;
                Ok(Self::Agent(AgentInvoke::AgentEventsSince {
                    since_seq: p.since_seq,
                    limit: p.limit,
                }))
            }
            "rename_agent" => {
                let p: proto::AgentIdNameParams = parse_params(params)?;
                Ok(Self::Agent(AgentInvoke::RenameAgent {
                    agent_id: p.agent_id,
                    name: p.name,
                }))
            }
            "list_agent_definitions" => {
                let p: proto::WorkspacePathOptionalParams = parse_params(params)?;
                Ok(Self::Definitions(DefinitionsInvoke::ListAgentDefinitions {
                    workspace_path: p.workspace_path,
                }))
            }
            "save_agent_definition" => {
                let p: proto::SaveAgentDefinitionParams = parse_params(params)?;
                Ok(Self::Definitions(DefinitionsInvoke::SaveAgentDefinition {
                    definition_json: p.definition_json,
                    scope: p.scope,
                    workspace_path: p.workspace_path,
                }))
            }
            "delete_agent_definition" => {
                let p: proto::ScopedIdParams = parse_params(params)?;
                Ok(Self::Definitions(
                    DefinitionsInvoke::DeleteAgentDefinition {
                        id: p.id,
                        scope: p.scope,
                        workspace_path: p.workspace_path,
                    },
                ))
            }
            "list_available_skills" => {
                let p: proto::WorkspacePathOptionalParams = parse_params(params)?;
                Ok(Self::Definitions(DefinitionsInvoke::ListAvailableSkills {
                    workspace_path: p.workspace_path,
                }))
            }
            "discover_git_repos" => {
                let p: proto::WorkspaceDirParams = parse_params(params)?;
                Ok(Self::Git(GitInvoke::DiscoverRepos {
                    workspace_dir: p.workspace_dir,
                }))
            }
            "git_current_branch" => {
                let p: proto::WorkingDirParams = parse_params(params)?;
                Ok(Self::Git(GitInvoke::CurrentBranch {
                    working_dir: p.working_dir,
                }))
            }
            "git_status" => {
                let p: proto::WorkingDirParams = parse_params(params)?;
                Ok(Self::Git(GitInvoke::Status {
                    working_dir: p.working_dir,
                }))
            }
            "git_stage" => {
                let p: proto::WorkingDirPathsParams = parse_params(params)?;
                Ok(Self::Git(GitInvoke::Stage {
                    working_dir: p.working_dir,
                    paths: p.paths,
                }))
            }
            "git_unstage" => {
                let p: proto::WorkingDirPathsParams = parse_params(params)?;
                Ok(Self::Git(GitInvoke::Unstage {
                    working_dir: p.working_dir,
                    paths: p.paths,
                }))
            }
            "git_commit" => {
                let p: proto::WorkingDirMessageParams = parse_params(params)?;
                Ok(Self::Git(GitInvoke::Commit {
                    working_dir: p.working_dir,
                    message: p.message,
                }))
            }
            "git_diff" => {
                let p: proto::WorkingDirPathStagedParams = parse_params(params)?;
                Ok(Self::Git(GitInvoke::Diff {
                    working_dir: p.working_dir,
                    path: p.path,
                    staged: p.staged,
                }))
            }
            "git_diff_base_content" => {
                let p: proto::WorkingDirPathStagedParams = parse_params(params)?;
                Ok(Self::Git(GitInvoke::DiffBaseContent {
                    working_dir: p.working_dir,
                    path: p.path,
                    staged: p.staged,
                }))
            }
            "git_discard" => {
                let p: proto::WorkingDirPathsParams = parse_params(params)?;
                Ok(Self::Git(GitInvoke::Discard {
                    working_dir: p.working_dir,
                    paths: p.paths,
                }))
            }
            "git_worktree_add" => {
                let p: proto::WorkingDirPathBranchParams = parse_params(params)?;
                Ok(Self::Git(GitInvoke::WorktreeAdd {
                    working_dir: p.working_dir,
                    path: p.path,
                    branch: p.branch,
                }))
            }
            "git_worktree_remove" => {
                let p: proto::WorkingDirPathParams = parse_params(params)?;
                Ok(Self::Git(GitInvoke::WorktreeRemove {
                    working_dir: p.working_dir,
                    path: p.path,
                }))
            }
            "list_directory" => {
                let p: proto::ListDirectoryParams = parse_params(params)?;
                Ok(Self::File(FileInvoke::ListDirectory {
                    path: p.path,
                    show_hidden: p.show_hidden,
                }))
            }
            "read_file_content" => {
                let p: proto::PathParams = parse_params(params)?;
                Ok(Self::File(FileInvoke::ReadFileContent { path: p.path }))
            }
            "sync_file_watch_paths" => {
                let p: proto::PathsParams = parse_params(params)?;
                Ok(Self::File(FileInvoke::SyncFileWatchPaths {
                    paths: p.paths,
                }))
            }
            "watch_workspace_dir" => {
                let p: proto::PathParams = parse_params(params)?;
                Ok(Self::File(FileInvoke::WatchWorkspaceDir { path: p.path }))
            }
            "unwatch_workspace_dir" => Ok(Self::File(FileInvoke::UnwatchWorkspaceDir)),
            "create_terminal" => {
                let p: proto::WorkspacePathParams = parse_params(params)?;
                Ok(Self::Terminal(TerminalInvoke::Create {
                    workspace_path: p.workspace_path,
                }))
            }
            "write_terminal" => {
                let p: proto::TerminalWriteParams = parse_params(params)?;
                Ok(Self::Terminal(TerminalInvoke::Write {
                    terminal_id: p.terminal_id,
                    data: p.data,
                }))
            }
            "resize_terminal" => {
                let p: proto::TerminalResizeParams = parse_params(params)?;
                Ok(Self::Terminal(TerminalInvoke::Resize {
                    terminal_id: p.terminal_id,
                    cols: p.cols,
                    rows: p.rows,
                }))
            }
            "close_terminal" => {
                let p: proto::TerminalIdParams = parse_params(params)?;
                Ok(Self::Terminal(TerminalInvoke::Close {
                    terminal_id: p.terminal_id,
                }))
            }
            "create_admin_subprocess" => {
                let p: proto::CreateAdminSubprocessParams = parse_params(params)?;
                Ok(Self::Admin(AdminInvoke::CreateSubprocess {
                    workspace_roots: p.workspace_roots,
                    backend_kind: p.backend_kind.map(|kind| kind.as_str().to_string()),
                }))
            }
            "close_admin_subprocess" => {
                let p: proto::AdminIdParams = parse_params(params)?;
                Ok(Self::Admin(AdminInvoke::CloseSubprocess {
                    admin_id: p.admin_id,
                }))
            }
            "admin_list_sessions" => {
                let p: proto::AdminIdParams = parse_params(params)?;
                Ok(Self::Admin(AdminInvoke::ListSessions {
                    admin_id: p.admin_id,
                }))
            }
            "admin_get_settings" => {
                let p: proto::AdminIdParams = parse_params(params)?;
                Ok(Self::Admin(AdminInvoke::GetSettings {
                    admin_id: p.admin_id,
                }))
            }
            "admin_update_settings" => {
                let p: proto::AdminUpdateSettingsParams = parse_params(params)?;
                Ok(Self::Admin(AdminInvoke::UpdateSettings {
                    admin_id: p.admin_id,
                    settings: p.settings,
                }))
            }
            "admin_list_profiles" => {
                let p: proto::AdminIdParams = parse_params(params)?;
                Ok(Self::Admin(AdminInvoke::ListProfiles {
                    admin_id: p.admin_id,
                }))
            }
            "admin_switch_profile" => {
                let p: proto::AdminProfileParams = parse_params(params)?;
                Ok(Self::Admin(AdminInvoke::SwitchProfile {
                    admin_id: p.admin_id,
                    profile_name: p.profile_name,
                }))
            }
            "admin_get_module_schemas" => {
                let p: proto::AdminIdParams = parse_params(params)?;
                Ok(Self::Admin(AdminInvoke::GetModuleSchemas {
                    admin_id: p.admin_id,
                }))
            }
            "admin_delete_session" => {
                let p: proto::AdminSessionParams = parse_params(params)?;
                Ok(Self::Admin(AdminInvoke::DeleteSession {
                    admin_id: p.admin_id,
                    session_id: p.session_id,
                }))
            }
            "query_backend_usage" => {
                let p: proto::QueryBackendUsageParams = parse_params(params)?;
                Ok(Self::Backend(BackendInvoke::QueryUsage {
                    backend_kind: p.backend_kind.as_str().to_string(),
                    host_id: p.host_id,
                }))
            }
            "check_backend_dependencies" => Ok(Self::Backend(BackendInvoke::CheckDependencies)),
            "install_backend_dependency" => {
                let p: proto::BackendKindStringParams = parse_params(params)?;
                Ok(Self::Backend(BackendInvoke::InstallDependency {
                    backend_kind: p.backend_kind,
                }))
            }
            "list_session_records" => {
                let p: proto::ListSessionRecordsParams = parse_params(params)?;
                Ok(Self::SessionRecord(SessionRecordInvoke::List {
                    workspace_root: p.workspace_root,
                    host_id: p.host_id,
                }))
            }
            "get_session_id" => {
                let p: proto::ConversationIdParams = parse_params(params)?;
                Ok(Self::SessionRecord(SessionRecordInvoke::GetSessionId {
                    agent_id: p.conversation_id,
                }))
            }
            "rename_session" => {
                let p: proto::HostScopedIdNameParams = parse_params(params)?;
                Ok(Self::SessionRecord(SessionRecordInvoke::Rename {
                    id: p.id,
                    name: p.name,
                    host_id: p.host_id,
                }))
            }
            "set_session_alias" => {
                let p: proto::HostScopedIdAliasParams = parse_params(params)?;
                Ok(Self::SessionRecord(SessionRecordInvoke::SetAlias {
                    id: p.id,
                    alias: p.alias,
                    host_id: p.host_id,
                }))
            }
            "delete_session_record" => {
                let p: proto::HostScopedIdParams = parse_params(params)?;
                Ok(Self::SessionRecord(SessionRecordInvoke::Delete {
                    id: p.id,
                    host_id: p.host_id,
                }))
            }
            "list_projects" => {
                let p: proto::HostSelectorParams = parse_params(params)?;
                Ok(Self::Project(ProjectInvoke::List { host: p.host }))
            }
            "add_project" => {
                let p: proto::AddProjectParams = parse_params(params)?;
                Ok(Self::Project(ProjectInvoke::Add {
                    host: p.host,
                    workspace_path: p.workspace_path,
                    name: p.name,
                }))
            }
            "add_project_workbench" => {
                let p: proto::AddProjectWorkbenchParams = parse_params(params)?;
                Ok(Self::Project(ProjectInvoke::AddWorkbench {
                    host: p.host,
                    parent_project_id: p.parent_project_id,
                    workspace_path: p.workspace_path,
                    name: p.name,
                    kind: p.kind,
                }))
            }
            "remove_project" => {
                let p: proto::HostProjectIdParams = parse_params(params)?;
                Ok(Self::Project(ProjectInvoke::Remove {
                    host: p.host,
                    id: p.id,
                }))
            }
            "remove_project_by_workspace_path" => {
                let p: proto::WorkspacePathParams = parse_params(params)?;
                Ok(Self::Project(ProjectInvoke::RemoveByWorkspacePath {
                    workspace_path: p.workspace_path,
                }))
            }
            "rename_project" => {
                let p: proto::RenameProjectParams = parse_params(params)?;
                Ok(Self::Project(ProjectInvoke::Rename {
                    host: p.host,
                    id: p.id,
                    name: p.name,
                }))
            }
            "update_project_roots" => {
                let p: proto::UpdateProjectRootsParams = parse_params(params)?;
                Ok(Self::Project(ProjectInvoke::UpdateRoots {
                    host: p.host,
                    id: p.id,
                    roots: p.roots,
                }))
            }
            "register_git_workbench" => {
                let p: proto::RegisterGitWorkbenchParams = parse_params(params)?;
                Ok(Self::Project(ProjectInvoke::RegisterGitWorkbench {
                    parent_workspace_path: p.parent_workspace_path,
                    worktree_path: p.worktree_path,
                    branch: p.branch,
                }))
            }
            "server_status" => Ok(Self::System(SystemInvoke::ServerStatus)),
            "set_default_backend" => {
                let p: proto::BackendStringParams = parse_params(params)?;
                Ok(Self::System(SystemInvoke::SetDefaultBackend {
                    backend: p.backend,
                }))
            }
            "set_disabled_backends" => {
                let p: proto::BackendsParams = parse_params(params)?;
                Ok(Self::System(SystemInvoke::SetDisabledBackends {
                    backends: p.backends,
                }))
            }
            "get_mcp_http_server_settings" => {
                Ok(Self::System(SystemInvoke::GetMcpHttpServerSettings))
            }
            "set_mcp_http_server_enabled" => {
                let p: proto::EnabledParams = parse_params(params)?;
                Ok(Self::System(SystemInvoke::SetMcpHttpServerEnabled {
                    enabled: p.enabled,
                }))
            }
            "get_driver_mcp_http_server_settings" => {
                Ok(Self::System(SystemInvoke::GetDriverMcpHttpServerSettings))
            }
            "set_driver_mcp_http_server_enabled" => {
                let p: proto::EnabledParams = parse_params(params)?;
                Ok(Self::System(SystemInvoke::SetDriverMcpHttpServerEnabled {
                    enabled: p.enabled,
                }))
            }
            "set_driver_mcp_http_server_autoload_enabled" => {
                let p: proto::EnabledParams = parse_params(params)?;
                Ok(Self::System(
                    SystemInvoke::SetDriverMcpHttpServerAutoloadEnabled { enabled: p.enabled },
                ))
            }
            "set_mcp_control_enabled" => {
                let p: proto::EnabledParams = parse_params(params)?;
                Ok(Self::System(SystemInvoke::SetMcpControlEnabled {
                    enabled: p.enabled,
                }))
            }
            "get_remote_control_settings" => {
                Ok(Self::System(SystemInvoke::GetRemoteControlSettings))
            }
            "set_remote_control_enabled" => {
                let p: proto::EnabledParams = parse_params(params)?;
                Ok(Self::System(SystemInvoke::SetRemoteControlEnabled {
                    enabled: p.enabled,
                }))
            }
            "list_hosts" => Ok(Self::System(SystemInvoke::ListHosts)),
            "add_host" => {
                let p: proto::AddHostParams = parse_params(params)?;
                Ok(Self::System(SystemInvoke::AddHost {
                    label: p.label,
                    hostname: p.hostname,
                }))
            }
            "remove_host" => {
                let p: proto::IdParams = parse_params(params)?;
                Ok(Self::System(SystemInvoke::RemoveHost { id: p.id }))
            }
            "update_host_label" => {
                let p: proto::IdLabelParams = parse_params(params)?;
                Ok(Self::System(SystemInvoke::UpdateHostLabel {
                    id: p.id,
                    label: p.label,
                }))
            }
            "update_host_enabled_backends" => {
                let p: proto::IdBackendsParams = parse_params(params)?;
                Ok(Self::System(SystemInvoke::UpdateHostEnabledBackends {
                    id: p.id,
                    backends: p.backends,
                }))
            }
            "update_host_default_backend" => {
                let p: proto::IdBackendParams = parse_params(params)?;
                Ok(Self::System(SystemInvoke::UpdateHostDefaultBackend {
                    id: p.id,
                    backend: p.backend,
                }))
            }
            "get_host_for_workspace" => {
                let p: proto::WorkspacePathParams = parse_params(params)?;
                Ok(Self::System(SystemInvoke::GetHostForWorkspace {
                    workspace_path: p.workspace_path,
                }))
            }
            "list_active_conversations" => Ok(Self::System(SystemInvoke::ListActiveAgents)),
            "list_workflows" => {
                let p: proto::WorkspacePathOptionalParams = parse_params(params)?;
                Ok(Self::System(SystemInvoke::ListWorkflows {
                    workspace_path: p.workspace_path,
                }))
            }
            "save_workflow" => {
                let p: proto::SaveWorkflowParams = parse_params(params)?;
                Ok(Self::System(SystemInvoke::SaveWorkflow {
                    workflow_json: p.workflow_json,
                    scope: p.scope,
                    workspace_path: p.workspace_path,
                }))
            }
            "delete_workflow" => {
                let p: proto::ScopedIdParams = parse_params(params)?;
                Ok(Self::System(SystemInvoke::DeleteWorkflow {
                    id: p.id,
                    scope: p.scope,
                    workspace_path: p.workspace_path,
                }))
            }
            "run_shell_command" => {
                let p: proto::RunShellCommandParams = parse_params(params)?;
                Ok(Self::System(SystemInvoke::RunShellCommand {
                    command: p.command,
                    cwd: p.cwd,
                }))
            }
            _ => Err(format!("Unknown command: {command}")),
        }
    }

    pub fn session_lane_id(&self) -> Option<&str> {
        match self {
            Self::Conversation(
                ConversationInvoke::SendMessage { agent_id, .. }
                | ConversationInvoke::CancelAgent { agent_id }
                | ConversationInvoke::CloseAgent { agent_id }
                | ConversationInvoke::ResumeSession { agent_id, .. }
                | ConversationInvoke::DeleteSession { agent_id, .. }
                | ConversationInvoke::SwitchProfile { agent_id, .. }
                | ConversationInvoke::UpdateSettings { agent_id, .. },
            ) => Some(agent_id.as_str()),
            _ => None,
        }
    }

    pub fn agent_lane_id(&self) -> Option<&str> {
        match self {
            Self::Agent(
                AgentInvoke::SendAgentMessage { agent_id, .. }
                | AgentInvoke::InterruptAgent { agent_id }
                | AgentInvoke::TerminateAgent { agent_id }
                | AgentInvoke::CancelAgent { agent_id }
                | AgentInvoke::RenameAgent { agent_id, .. },
            ) => Some(agent_id.as_str()),
            _ => None,
        }
    }

    pub fn terminal_lane_id(&self) -> Option<u64> {
        match self {
            Self::Terminal(
                TerminalInvoke::Write { terminal_id, .. }
                | TerminalInvoke::Resize { terminal_id, .. }
                | TerminalInvoke::Close { terminal_id },
            ) => Some(*terminal_id),
            _ => None,
        }
    }
}

pub trait InvokeHandler: Send + Sync {
    fn handle_conversation<'a>(
        &'a self,
        request: ConversationInvoke,
    ) -> BoxFuture<'a, Result<Value, String>>;

    fn handle_agent<'a>(&'a self, request: AgentInvoke) -> BoxFuture<'a, Result<Value, String>>;

    fn handle_definitions<'a>(
        &'a self,
        request: DefinitionsInvoke,
    ) -> BoxFuture<'a, Result<Value, String>>;

    fn handle_git<'a>(&'a self, request: GitInvoke) -> BoxFuture<'a, Result<Value, String>>;

    fn handle_file<'a>(&'a self, request: FileInvoke) -> BoxFuture<'a, Result<Value, String>>;

    fn handle_terminal<'a>(
        &'a self,
        request: TerminalInvoke,
    ) -> BoxFuture<'a, Result<Value, String>>;

    fn handle_admin<'a>(&'a self, request: AdminInvoke) -> BoxFuture<'a, Result<Value, String>>;

    fn handle_backend<'a>(&'a self, request: BackendInvoke)
        -> BoxFuture<'a, Result<Value, String>>;

    fn handle_session_record<'a>(
        &'a self,
        request: SessionRecordInvoke,
    ) -> BoxFuture<'a, Result<Value, String>>;

    fn handle_project<'a>(&'a self, request: ProjectInvoke)
        -> BoxFuture<'a, Result<Value, String>>;

    fn handle_system<'a>(&'a self, request: SystemInvoke) -> BoxFuture<'a, Result<Value, String>>;
}

pub async fn dispatch_invoke<H: InvokeHandler>(
    handler: &H,
    request: InvokeRequest,
) -> Result<Value, String> {
    tracing::info!(
        target: "tyde_server::invoke",
        direction = "in",
        request = ?request,
        "Dispatching invoke request"
    );
    let result = match request {
        InvokeRequest::Conversation(request) => handler.handle_conversation(request).await,
        InvokeRequest::Agent(request) => handler.handle_agent(request).await,
        InvokeRequest::Definitions(request) => handler.handle_definitions(request).await,
        InvokeRequest::Git(request) => handler.handle_git(request).await,
        InvokeRequest::File(request) => handler.handle_file(request).await,
        InvokeRequest::Terminal(request) => handler.handle_terminal(request).await,
        InvokeRequest::Admin(request) => handler.handle_admin(request).await,
        InvokeRequest::Backend(request) => handler.handle_backend(request).await,
        InvokeRequest::SessionRecord(request) => handler.handle_session_record(request).await,
        InvokeRequest::Project(request) => handler.handle_project(request).await,
        InvokeRequest::System(request) => handler.handle_system(request).await,
    };
    match &result {
        Ok(response) => tracing::info!(
            target: "tyde_server::invoke",
            direction = "out",
            response = %response,
            "Invoke request completed"
        ),
        Err(error) => tracing::info!(
            target: "tyde_server::invoke",
            direction = "out",
            error = %error,
            "Invoke request failed"
        ),
    }
    result
}

fn parse_params<T: DeserializeOwned>(params: Value) -> Result<T, String> {
    serde_json::from_value(params).map_err(|e| e.to_string())
}
