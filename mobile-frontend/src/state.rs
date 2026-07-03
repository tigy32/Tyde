use std::collections::{HashMap, HashSet};

use leptos::prelude::*;
pub use mobile_shell_types::{
    LocalHostId, MobilePairingPreview, MobileShellError, PairedHostConnectionStatus,
    PairedHostSummary,
};
use protocol::types::AgentCompactNotifyPayload;
use protocol::{
    AgentId, AgentOrigin, BackendKind, BackendSetupInfo, ChatMessage, ChatMessageId, CustomAgent,
    CustomAgentId, DiffContextMode, HostAbsPath, HostBrowseEntriesPayload, HostBrowseErrorPayload,
    HostBrowseOpenedPayload, HostSettings, McpServerConfig, McpServerId, MessageMetadataUpdateData,
    Project, ProjectDiffScope, ProjectFileContentsPayload, ProjectGitDiffFile, ProjectId,
    ProjectPath, ProjectRootGitStatus, ProjectRootListing, ProjectRootPath, QueuedMessageEntry,
    Review, ReviewErrorPayload, ReviewId, ReviewSummary, SessionId, SessionSchemaEntry,
    SessionSettingsValues, SessionSummary, Skill, SkillId, Steering, SteeringId, StreamPath,
    TaskList, Team, TeamCompactNotifyPayload, TeamDraft, TeamDraftId, TeamMember,
    TeamMemberBindingPayload, TeamMemberId, TeamMemberShuffleSuggestion, TeamPresetCatalog,
    ToolExecutionCompletedData, ToolRequest,
};

// ── Tool output viewing mode ───────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolOutputMode {
    Summary,
    Compact,
    Full,
}

// ── Connection status ──────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
pub enum ConnectionStatus {
    Disconnected,
    Connecting,
    Connected,
    Error(String),
}

impl From<PairedHostConnectionStatus> for ConnectionStatus {
    fn from(status: PairedHostConnectionStatus) -> Self {
        match status {
            PairedHostConnectionStatus::Connecting => ConnectionStatus::Connecting,
            PairedHostConnectionStatus::Connected => ConnectionStatus::Connected,
            PairedHostConnectionStatus::Disconnected { .. } => ConnectionStatus::Disconnected,
            PairedHostConnectionStatus::Failed { message, .. } => ConnectionStatus::Error(message),
        }
    }
}

// ── App mode + pairing screens ─────────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
pub enum PairingScreen {
    Scanner,
    ManualPaste,
    Confirm {
        qr_uri: String,
        preview: MobilePairingPreview,
    },
    InProgress {
        qr_uri: String,
        preview: MobilePairingPreview,
    },
    Failed {
        message: String,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub enum AppMode {
    /// User has zero paired hosts.
    Onboarding,
    /// User has at least one paired host but is not currently in the pairing
    /// flow. The picker / per-host workspace renders here.
    Workspace,
    /// Pairing flow is on-screen.
    Pairing(PairingScreen),
}

// ── Refs ───────────────────────────────────────────────────────────────

/// Composite key for per-agent state (chat, tasks, streaming, etc).
/// Combines `LocalHostId` with `AgentId` so two paired hosts that happen to
/// generate colliding agent identifiers stay isolated.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct AgentRef {
    pub local_host_id: LocalHostId,
    pub agent_id: AgentId,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ActiveProjectRef {
    pub local_host_id: LocalHostId,
    pub project_id: ProjectId,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ProjectFileRef {
    pub local_host_id: LocalHostId,
    pub project_id: ProjectId,
    pub path: ProjectPath,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProjectFileState {
    pub path: ProjectPath,
    pub contents: Option<String>,
    pub is_binary: bool,
}

impl From<ProjectFileContentsPayload> for ProjectFileState {
    fn from(payload: ProjectFileContentsPayload) -> Self {
        Self {
            path: payload.path,
            contents: payload.contents,
            is_binary: payload.is_binary,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ProjectDiffRef {
    pub local_host_id: LocalHostId,
    pub project_id: ProjectId,
    pub root: ProjectRootPath,
    pub scope: ProjectDiffScope,
    pub path: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProjectDiffState {
    pub root: ProjectRootPath,
    pub scope: ProjectDiffScope,
    pub path: Option<String>,
    pub context_mode: DiffContextMode,
    pub pending: bool,
    pub files: Vec<ProjectGitDiffFile>,
}

impl ProjectDiffState {
    pub fn for_request(
        previous: Option<&ProjectDiffState>,
        root: ProjectRootPath,
        scope: ProjectDiffScope,
        path: Option<String>,
        context_mode: DiffContextMode,
    ) -> Self {
        let files = previous
            .filter(|existing| existing.context_mode == context_mode)
            .map(|existing| existing.files.clone())
            .unwrap_or_default();
        Self {
            root,
            scope,
            path,
            context_mode,
            pending: true,
            files,
        }
    }
}

pub fn reduce_project_diff_response(
    current: Option<&ProjectDiffState>,
    payload: protocol::ProjectGitDiffPayload,
) -> Option<ProjectDiffState> {
    if current.is_some_and(|state| state.pending && state.context_mode != payload.context_mode) {
        return None;
    }
    Some(ProjectDiffState {
        root: payload.root,
        scope: payload.scope,
        path: payload.path,
        context_mode: payload.context_mode,
        pending: false,
        files: payload.files,
    })
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ReviewRef {
    pub local_host_id: LocalHostId,
    pub review_id: ReviewId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActiveAgentRef {
    pub local_host_id: LocalHostId,
    pub agent_id: AgentId,
}

impl ActiveAgentRef {
    pub fn as_agent_ref(&self) -> AgentRef {
        AgentRef {
            local_host_id: self.local_host_id.clone(),
            agent_id: self.agent_id.clone(),
        }
    }
}

// ── Agent info ─────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
pub struct AgentInfo {
    pub local_host_id: LocalHostId,
    pub agent_id: AgentId,
    pub name: String,
    pub origin: AgentOrigin,
    pub backend_kind: BackendKind,
    pub workspace_roots: Vec<String>,
    pub project_id: Option<ProjectId>,
    pub parent_agent_id: Option<AgentId>,
    pub session_id: Option<SessionId>,
    pub custom_agent_id: Option<CustomAgentId>,
    pub created_at_ms: u64,
    pub instance_stream: StreamPath,
    pub started: bool,
    pub fatal_error: Option<String>,
}

impl AgentInfo {
    pub fn agent_ref(&self) -> AgentRef {
        AgentRef {
            local_host_id: self.local_host_id.clone(),
            agent_id: self.agent_id.clone(),
        }
    }
}

// ── Chat types ─────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct ChatMessageEntry {
    pub message: ChatMessage,
    pub tool_requests: Vec<ToolRequestEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionHistoryState {
    pub message_count: u32,
    pub oldest_seq: Option<u64>,
    pub has_more_before: bool,
    pub loading: bool,
}

#[derive(Clone, Debug)]
pub struct ToolRequestEntry {
    pub request: ToolRequest,
    pub result: Option<ToolExecutionCompletedData>,
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

// ── Project/session info ───────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
pub struct ProjectInfo {
    pub local_host_id: LocalHostId,
    pub project: Project,
}

/// Orders the project list the way the server's `ordered_projects` emits
/// it: hosts first, then each host's top-level projects by `sort_order`,
/// with every parent's git-workbench children listed directly beneath it
/// (children ordered by their own per-parent `sort_order`). Workbench
/// children carry an independent `sort_order` sequence starting at 0, so
/// a flat sort by raw `sort_order` would interleave them among top-level
/// projects. Updates arrive as single-project upserts
/// (`ProjectNotify::Upsert`, project bootstraps) as well as full
/// snapshots, so the grouped order is re-derived locally instead of
/// trusting arrival order. A workbench whose parent hasn't arrived yet
/// sorts after every known top-level project (grouped with any orphan
/// siblings of the same parent) until the parent's upsert lands and the
/// next re-sort slots it into place.
pub fn sort_project_infos(projects: &mut [ProjectInfo]) {
    // (top-level sort_order, top-level name, top-level id) for parent
    // lookup, keyed per host so colliding ids across hosts stay isolated.
    let top_level: HashMap<(LocalHostId, ProjectId), (u64, String)> = projects
        .iter()
        .filter(|info| !info.project.is_workbench())
        .map(|info| {
            (
                (info.local_host_id.clone(), info.project.id.clone()),
                (info.project.sort_order, info.project.name.clone()),
            )
        })
        .collect();

    projects.sort_by_cached_key(|info| {
        let host = info.local_host_id.0.clone();
        let own = (
            info.project.sort_order,
            info.project.name.clone(),
            info.project.id.0.clone(),
        );
        match info.project.parent_project_id() {
            None => {
                let (order, name, id) = own;
                // Top-level rows sort by their own key and come before
                // their children (`is_child = 0`).
                (
                    host,
                    order,
                    name,
                    id,
                    0u8,
                    0u64,
                    String::new(),
                    String::new(),
                )
            }
            Some(parent_id) => {
                let key = (info.local_host_id.clone(), parent_id.clone());
                let (parent_order, parent_name) = top_level
                    .get(&key)
                    .cloned()
                    // Orphan workbench: parent not (yet) in the list.
                    // Push it after all known top-level groups.
                    .unwrap_or((u64::MAX, String::new()));
                let (order, name, id) = own;
                (
                    host,
                    parent_order,
                    parent_name,
                    parent_id.0.clone(),
                    1u8,
                    order,
                    name,
                    id,
                )
            }
        }
    });
}

#[derive(Clone, Debug, PartialEq)]
pub struct SessionInfo {
    pub local_host_id: LocalHostId,
    pub summary: SessionSummary,
}

#[derive(Clone, Debug)]
pub struct HostBrowseSession {
    pub local_host_id: LocalHostId,
    pub stream: StreamPath,
    pub opened: Option<HostBrowseOpenedPayload>,
    pub entries_by_path: HashMap<HostAbsPath, HostBrowseEntriesPayload>,
    pub latest_error: Option<HostBrowseErrorPayload>,
}

// ── Navigation ─────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, Eq, Copy)]
pub enum MobileTab {
    Home,
    Agents,
    Sessions,
    Projects,
    Settings,
}

// ── App state ──────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct AppState {
    // Top-level routing
    pub app_mode: RwSignal<AppMode>,
    pub active_local_host_id: RwSignal<Option<LocalHostId>>,

    // Multi-host
    pub paired_hosts: RwSignal<Vec<PairedHostSummary>>,
    pub connection_statuses: RwSignal<HashMap<LocalHostId, ConnectionStatus>>,
    /// Tracks the `connection_instance_id` of the MQTT connection for which
    /// the frontend last sent Hello.  Used to detect same-connection status
    /// replays (no re-Hello needed) vs. genuinely new connections.
    pub active_connection_instance_ids: RwSignal<HashMap<LocalHostId, u64>>,
    pub host_streams: RwSignal<HashMap<LocalHostId, StreamPath>>,
    pub host_settings_by_host: RwSignal<HashMap<LocalHostId, HostSettings>>,
    pub command_errors_by_host: RwSignal<HashMap<LocalHostId, String>>,
    pub backend_setup_by_host: RwSignal<HashMap<LocalHostId, Vec<BackendSetupInfo>>>,
    pub session_schemas_by_host:
        RwSignal<HashMap<LocalHostId, HashMap<BackendKind, SessionSchemaEntry>>>,
    pub custom_agents_by_host: RwSignal<HashMap<LocalHostId, HashMap<CustomAgentId, CustomAgent>>>,
    pub mcp_servers_by_host: RwSignal<HashMap<LocalHostId, HashMap<McpServerId, McpServerConfig>>>,
    pub steering_by_host: RwSignal<HashMap<LocalHostId, HashMap<SteeringId, Steering>>>,
    pub skills_by_host: RwSignal<HashMap<LocalHostId, HashMap<SkillId, Skill>>>,

    // Mobile shell error notification
    pub mobile_shell_error: RwSignal<Option<MobileShellError>>,

    // Tab navigation within the per-host workspace
    pub active_tab: RwSignal<MobileTab>,
    pub viewing_chat: RwSignal<bool>,

    // Projects
    pub projects: RwSignal<Vec<ProjectInfo>>,
    pub active_project: RwSignal<Option<ActiveProjectRef>>,
    pub file_tree: RwSignal<HashMap<(LocalHostId, ProjectId), Vec<ProjectRootListing>>>,
    pub git_status: RwSignal<HashMap<(LocalHostId, ProjectId), Vec<ProjectRootGitStatus>>>,
    pub project_file_contents: RwSignal<HashMap<ProjectFileRef, ProjectFileState>>,
    pub project_diffs: RwSignal<HashMap<ProjectDiffRef, ProjectDiffState>>,
    pub review_summaries: RwSignal<HashMap<(LocalHostId, ProjectId), Vec<ReviewSummary>>>,
    pub reviews: RwSignal<HashMap<ReviewRef, Review>>,
    pub review_errors: RwSignal<HashMap<ReviewRef, ReviewErrorPayload>>,
    pub review_streams: RwSignal<HashMap<ReviewRef, StreamPath>>,

    // Agents & Chat
    pub agents: RwSignal<Vec<AgentInfo>>,
    pub active_agent: RwSignal<Option<ActiveAgentRef>>,
    /// Agents whose `AgentBootstrap` has been requested or received on this
    /// frontend connection. Mobile asks for bootstraps lazily when a chat is
    /// opened instead of replaying every transcript on startup.
    pub agent_load_requests: RwSignal<HashSet<AgentRef>>,
    /// Agents whose `AgentBootstrap` snapshot has actually arrived. Distinct
    /// from `agent_load_requests`, which latches as soon as a load is sent —
    /// this only flips once the transcript snapshot lands, so a chat opened on
    /// a slow link can show a loading spinner instead of a premature "empty"
    /// state in the window between the request and its bootstrap reply.
    pub agent_loaded: RwSignal<HashSet<AgentRef>>,
    pub chat_messages: RwSignal<HashMap<AgentRef, Vec<ChatMessageEntry>>>,
    /// Per-agent index from server-issued `ChatMessageId` to the position
    /// in `chat_messages[agent]` that carries it. Populated when a row is
    /// pushed (live `MessageAdded`/`StreamEnd`, replayed bootstrap events)
    /// if the message's `message_id` is `Some`, and read when a
    /// `MessageMetadataUpdated` event lands so the existing row can be
    /// patched in place. Cleared anywhere `chat_messages` is cleared
    /// (host runtime reset, agent close, agent bootstrap snapshot).
    pub chat_message_index: RwSignal<HashMap<AgentRef, HashMap<ChatMessageId, usize>>>,
    /// Server-owned prior-history availability for each agent. The server
    /// sends only this indicator in `AgentBootstrap`; actual prior transcript
    /// rows are fetched explicitly with `FetchSessionHistory` and prepended
    /// when `SessionHistory` arrives.
    pub session_history: RwSignal<HashMap<AgentRef, SessionHistoryState>>,
    pub streaming_text: RwSignal<HashMap<AgentRef, StreamingState>>,
    pub chat_input: RwSignal<String>,
    pub task_lists: RwSignal<HashMap<AgentRef, TaskList>>,
    pub agent_message_queue: RwSignal<HashMap<AgentRef, Vec<QueuedMessageEntry>>>,
    pub agent_turn_active: RwSignal<HashMap<AgentRef, bool>>,
    pub transient_events: RwSignal<HashMap<AgentRef, Vec<TransientEvent>>>,
    pub agent_session_settings: RwSignal<HashMap<AgentRef, SessionSettingsValues>>,
    pub agent_compactions: RwSignal<HashMap<AgentRef, AgentCompactNotifyPayload>>,

    // Teams
    pub teams_by_host: RwSignal<HashMap<LocalHostId, HashMap<protocol::TeamId, Team>>>,
    pub team_members_by_host: RwSignal<HashMap<LocalHostId, HashMap<TeamMemberId, TeamMember>>>,
    pub team_bindings_by_host:
        RwSignal<HashMap<LocalHostId, HashMap<TeamMemberId, TeamMemberBindingPayload>>>,
    pub team_compactions_by_host:
        RwSignal<HashMap<LocalHostId, HashMap<protocol::TeamId, TeamCompactNotifyPayload>>>,
    pub team_preset_catalog_by_host: RwSignal<HashMap<LocalHostId, TeamPresetCatalog>>,
    pub team_drafts_by_host: RwSignal<HashMap<LocalHostId, HashMap<TeamDraftId, TeamDraft>>>,
    pub team_shuffle_suggestions_by_host:
        RwSignal<HashMap<LocalHostId, HashMap<protocol::TeamId, TeamMemberShuffleSuggestion>>>,

    // Host filesystem browsing
    pub host_browses: RwSignal<HashMap<(LocalHostId, StreamPath), HostBrowseSession>>,

    // Sessions
    pub sessions: RwSignal<Vec<SessionInfo>>,

    // Draft state for new agent
    pub draft_backend_override: RwSignal<Option<BackendKind>>,
    pub draft_custom_agent_id: RwSignal<Option<CustomAgentId>>,
    pub draft_session_settings: RwSignal<SessionSettingsValues>,

    // Appearance
    pub theme: RwSignal<String>,
    pub tool_output_mode: RwSignal<ToolOutputMode>,
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

impl AppState {
    pub fn new() -> Self {
        Self {
            app_mode: RwSignal::new(AppMode::Onboarding),
            active_local_host_id: RwSignal::new(None),

            paired_hosts: RwSignal::new(Vec::new()),
            connection_statuses: RwSignal::new(HashMap::new()),
            active_connection_instance_ids: RwSignal::new(HashMap::new()),
            host_streams: RwSignal::new(HashMap::new()),
            host_settings_by_host: RwSignal::new(HashMap::new()),
            command_errors_by_host: RwSignal::new(HashMap::new()),
            backend_setup_by_host: RwSignal::new(HashMap::new()),
            session_schemas_by_host: RwSignal::new(HashMap::new()),
            custom_agents_by_host: RwSignal::new(HashMap::new()),
            mcp_servers_by_host: RwSignal::new(HashMap::new()),
            steering_by_host: RwSignal::new(HashMap::new()),
            skills_by_host: RwSignal::new(HashMap::new()),

            mobile_shell_error: RwSignal::new(None),

            active_tab: RwSignal::new(MobileTab::Home),
            viewing_chat: RwSignal::new(false),

            projects: RwSignal::new(Vec::new()),
            active_project: RwSignal::new(None),
            file_tree: RwSignal::new(HashMap::new()),
            git_status: RwSignal::new(HashMap::new()),
            project_file_contents: RwSignal::new(HashMap::new()),
            project_diffs: RwSignal::new(HashMap::new()),
            review_summaries: RwSignal::new(HashMap::new()),
            reviews: RwSignal::new(HashMap::new()),
            review_errors: RwSignal::new(HashMap::new()),
            review_streams: RwSignal::new(HashMap::new()),

            agents: RwSignal::new(Vec::new()),
            active_agent: RwSignal::new(None),
            agent_load_requests: RwSignal::new(HashSet::new()),
            agent_loaded: RwSignal::new(HashSet::new()),
            chat_messages: RwSignal::new(HashMap::new()),
            chat_message_index: RwSignal::new(HashMap::new()),
            session_history: RwSignal::new(HashMap::new()),
            streaming_text: RwSignal::new(HashMap::new()),
            chat_input: RwSignal::new(String::new()),
            task_lists: RwSignal::new(HashMap::new()),
            agent_message_queue: RwSignal::new(HashMap::new()),
            agent_turn_active: RwSignal::new(HashMap::new()),
            transient_events: RwSignal::new(HashMap::new()),
            agent_session_settings: RwSignal::new(HashMap::new()),
            agent_compactions: RwSignal::new(HashMap::new()),

            teams_by_host: RwSignal::new(HashMap::new()),
            team_members_by_host: RwSignal::new(HashMap::new()),
            team_bindings_by_host: RwSignal::new(HashMap::new()),
            team_compactions_by_host: RwSignal::new(HashMap::new()),
            team_preset_catalog_by_host: RwSignal::new(HashMap::new()),
            team_drafts_by_host: RwSignal::new(HashMap::new()),
            team_shuffle_suggestions_by_host: RwSignal::new(HashMap::new()),

            host_browses: RwSignal::new(HashMap::new()),

            sessions: RwSignal::new(Vec::new()),

            draft_backend_override: RwSignal::new(None),
            draft_custom_agent_id: RwSignal::new(None),
            draft_session_settings: RwSignal::new(SessionSettingsValues::default()),

            theme: RwSignal::new("dark".to_owned()),
            tool_output_mode: RwSignal::new(ToolOutputMode::Compact),
        }
    }

    pub fn host_stream_untracked(&self, host: &LocalHostId) -> Option<StreamPath> {
        self.host_streams.get_untracked().get(host).cloned()
    }

    /// Append a chat row for `agent_ref` and, if the message carries a
    /// server-issued `message_id`, register it in `chat_message_index` so
    /// a later `ChatEvent::MessageMetadataUpdated` can patch the row in
    /// place. The two writes are performed under separate signal updates
    /// because they target separate signals — there is no torn-state
    /// window for any single observer (each signal is internally
    /// consistent), and consumers only ever read the index after they
    /// can see the row that produced it.
    /// Drop server-provided prior-history state for a single agent. Call
    /// wherever `chat_messages` is cleared for that agent so a re-bootstrap
    /// starts from the server's new authoritative indicator.
    pub fn forget_session_history(&self, agent_ref: &AgentRef) {
        self.session_history.update(|map| {
            map.remove(agent_ref);
        });
    }

    pub fn push_chat_message_entry(&self, agent_ref: &AgentRef, entry: ChatMessageEntry) {
        let message_id = entry.message.message_id.clone();
        self.chat_messages.update(|messages| {
            messages.entry(agent_ref.clone()).or_default().push(entry);
        });
        if let Some(message_id) = message_id {
            let position = self
                .chat_messages
                .with_untracked(|m| m.get(agent_ref).map(|v| v.len().saturating_sub(1)));
            if let Some(position) = position {
                self.chat_message_index.update(|indexes| {
                    indexes
                        .entry(agent_ref.clone())
                        .or_default()
                        .insert(message_id, position);
                });
            }
        }
    }

    /// Patch the row matching `update.message_id` with whichever of
    /// `model_info` / `token_usage` / `context_breakdown` are `Some`.
    /// Same semantics as the desktop `apply_chat_message_metadata`.
    pub fn apply_chat_message_metadata(
        &self,
        agent_ref: &AgentRef,
        update: MessageMetadataUpdateData,
    ) {
        let position = self.chat_message_index.with_untracked(|indexes| {
            indexes
                .get(agent_ref)
                .and_then(|index| index.get(&update.message_id).copied())
        });
        let Some(position) = position else {
            log::warn!(
                "chat_event message_metadata_updated unknown message_id host={} agent_id={} message_id={}",
                agent_ref.local_host_id,
                agent_ref.agent_id,
                update.message_id
            );
            return;
        };
        let MessageMetadataUpdateData {
            message_id,
            model_info,
            token_usage,
            context_breakdown,
        } = update;
        let mut patched = false;
        self.chat_messages.update(|messages| {
            if let Some(agent_messages) = messages.get_mut(agent_ref)
                && let Some(entry) = agent_messages.get_mut(position)
                && entry.message.message_id.as_ref() == Some(&message_id)
            {
                if let Some(model_info) = model_info {
                    entry.message.model_info = Some(model_info);
                }
                if let Some(token_usage) = token_usage {
                    entry.message.token_usage = Some(token_usage);
                }
                if let Some(context_breakdown) = context_breakdown {
                    entry.message.context_breakdown = Some(context_breakdown);
                }
                patched = true;
            }
        });
        if !patched {
            log::warn!(
                "chat_event message_metadata_updated row missing after lookup host={} agent_id={} message_id={} position={}",
                agent_ref.local_host_id,
                agent_ref.agent_id,
                message_id,
                position
            );
        }
    }

    pub fn active_host_settings(&self) -> Option<HostSettings> {
        let host = self.active_local_host_id.get()?;
        self.host_settings_by_host.get().get(&host).cloned()
    }

    pub fn active_host_settings_untracked(&self) -> Option<HostSettings> {
        let host = self.active_local_host_id.get_untracked()?;
        self.host_settings_by_host
            .get_untracked()
            .get(&host)
            .cloned()
    }

    pub fn active_host_connection_status(&self) -> ConnectionStatus {
        let Some(host) = self.active_local_host_id.get() else {
            return ConnectionStatus::Disconnected;
        };
        self.connection_statuses
            .get()
            .get(&host)
            .cloned()
            .unwrap_or(ConnectionStatus::Disconnected)
    }

    /// True while the active host is connecting/connected but its
    /// `HostBootstrap` snapshot (the source of the agent + session lists)
    /// hasn't been applied yet. `HostSettings` is written as part of the
    /// bootstrap, so its presence is the same "snapshot landed" proxy
    /// `home_view` uses. Returns false once the snapshot lands (even if the
    /// lists are genuinely empty) and on a failed/disconnected host, so a
    /// loading spinner never outlives a connection that won't deliver data.
    pub fn host_snapshot_pending(&self) -> bool {
        let Some(host) = self.active_local_host_id.get() else {
            return false;
        };
        if self.host_settings_by_host.with(|m| m.contains_key(&host)) {
            return false;
        }
        matches!(
            self.active_host_connection_status(),
            ConnectionStatus::Connecting | ConnectionStatus::Connected
        )
    }

    pub fn active_host_command_error(&self) -> Option<String> {
        let host = self.active_local_host_id.get()?;
        self.command_errors_by_host.get().get(&host).cloned()
    }

    pub fn active_host_backend_setup(&self) -> Vec<BackendSetupInfo> {
        let Some(host) = self.active_local_host_id.get() else {
            return Vec::new();
        };
        self.backend_setup_by_host
            .get()
            .get(&host)
            .cloned()
            .unwrap_or_default()
    }

    pub fn active_host_custom_agents(&self) -> HashMap<CustomAgentId, CustomAgent> {
        let Some(host) = self.active_local_host_id.get() else {
            return HashMap::new();
        };
        self.custom_agents_by_host
            .get()
            .get(&host)
            .cloned()
            .unwrap_or_default()
    }

    pub fn active_paired_host(&self) -> Option<PairedHostSummary> {
        let host = self.active_local_host_id.get()?;
        self.paired_hosts
            .get()
            .into_iter()
            .find(|h| h.local_host_id == host)
    }

    /// Drops every per-host signal entry for `host`. Called when a host is
    /// forgotten (the user removed the pairing) or fully disconnects in a way
    /// that should clear cached snapshots.
    pub fn clear_host_runtime(&self, host: &LocalHostId) {
        self.active_connection_instance_ids.update(|m| {
            m.remove(host);
        });
        self.host_streams.update(|m| {
            m.remove(host);
        });
        self.host_settings_by_host.update(|m| {
            m.remove(host);
        });
        self.command_errors_by_host.update(|m| {
            m.remove(host);
        });
        self.backend_setup_by_host.update(|m| {
            m.remove(host);
        });
        self.session_schemas_by_host.update(|m| {
            m.remove(host);
        });
        self.custom_agents_by_host.update(|m| {
            m.remove(host);
        });
        self.mcp_servers_by_host.update(|m| {
            m.remove(host);
        });
        self.steering_by_host.update(|m| {
            m.remove(host);
        });
        self.skills_by_host.update(|m| {
            m.remove(host);
        });

        self.projects
            .update(|projects| projects.retain(|p| p.local_host_id != *host));
        self.agents
            .update(|agents| agents.retain(|a| a.local_host_id != *host));
        self.agent_load_requests.update(|m| {
            m.retain(|k| k.local_host_id != *host);
        });
        self.agent_loaded.update(|m| {
            m.retain(|k| k.local_host_id != *host);
        });
        self.sessions
            .update(|sessions| sessions.retain(|s| s.local_host_id != *host));

        self.file_tree.update(|m| {
            m.retain(|(h, _), _| h != host);
        });
        self.git_status.update(|m| {
            m.retain(|(h, _), _| h != host);
        });
        self.project_file_contents.update(|m| {
            m.retain(|k, _| k.local_host_id != *host);
        });
        self.project_diffs.update(|m| {
            m.retain(|k, _| k.local_host_id != *host);
        });
        self.review_summaries.update(|m| {
            m.retain(|(h, _), _| h != host);
        });
        self.reviews.update(|m| {
            m.retain(|k, _| k.local_host_id != *host);
        });
        self.review_errors.update(|m| {
            m.retain(|k, _| k.local_host_id != *host);
        });
        self.review_streams.update(|m| {
            m.retain(|k, _| k.local_host_id != *host);
        });

        self.chat_messages.update(|m| {
            m.retain(|k, _| k.local_host_id != *host);
        });
        self.chat_message_index.update(|m| {
            m.retain(|k, _| k.local_host_id != *host);
        });
        self.session_history.update(|m| {
            m.retain(|k, _| k.local_host_id != *host);
        });
        self.streaming_text.update(|m| {
            m.retain(|k, _| k.local_host_id != *host);
        });
        self.task_lists.update(|m| {
            m.retain(|k, _| k.local_host_id != *host);
        });
        self.agent_message_queue.update(|m| {
            m.retain(|k, _| k.local_host_id != *host);
        });
        self.agent_turn_active.update(|m| {
            m.retain(|k, _| k.local_host_id != *host);
        });
        self.transient_events.update(|m| {
            m.retain(|k, _| k.local_host_id != *host);
        });
        self.agent_session_settings.update(|m| {
            m.retain(|k, _| k.local_host_id != *host);
        });
        self.agent_compactions.update(|m| {
            m.retain(|k, _| k.local_host_id != *host);
        });
        self.teams_by_host.update(|m| {
            m.remove(host);
        });
        self.team_members_by_host.update(|m| {
            m.remove(host);
        });
        self.team_bindings_by_host.update(|m| {
            m.remove(host);
        });
        self.team_compactions_by_host.update(|m| {
            m.remove(host);
        });
        self.team_preset_catalog_by_host.update(|m| {
            m.remove(host);
        });
        self.team_drafts_by_host.update(|m| {
            m.remove(host);
        });
        self.team_shuffle_suggestions_by_host.update(|m| {
            m.remove(host);
        });
        self.host_browses.update(|m| {
            m.retain(|(h, _), _| h != host);
        });

        if self
            .active_project
            .get_untracked()
            .as_ref()
            .is_some_and(|active| active.local_host_id == *host)
        {
            self.active_project.set(None);
        }
        if self
            .active_agent
            .get_untracked()
            .as_ref()
            .is_some_and(|active| active.local_host_id == *host)
        {
            self.active_agent.set(None);
            self.viewing_chat.set(false);
        }
        if self
            .active_local_host_id
            .get_untracked()
            .as_ref()
            .is_some_and(|active| active == host)
        {
            self.active_local_host_id.set(None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::{GitBranchName, ProjectSource, WorkbenchRoot};

    fn top_level_project(host: &str, id: &str, name: &str, sort_order: u64) -> ProjectInfo {
        ProjectInfo {
            local_host_id: LocalHostId(host.to_owned()),
            project: Project {
                id: ProjectId(id.to_owned()),
                name: name.to_owned(),
                sort_order,
                source: ProjectSource::Standalone {
                    roots: vec![ProjectRootPath(format!("/x/{id}"))],
                },
            },
        }
    }

    fn workbench_project(
        host: &str,
        id: &str,
        name: &str,
        sort_order: u64,
        parent_id: &str,
    ) -> ProjectInfo {
        ProjectInfo {
            local_host_id: LocalHostId(host.to_owned()),
            project: Project {
                id: ProjectId(id.to_owned()),
                name: name.to_owned(),
                sort_order,
                source: ProjectSource::GitWorkbench {
                    parent_project_id: ProjectId(parent_id.to_owned()),
                    branch: GitBranchName(format!("branch-{id}")),
                    roots: vec![WorkbenchRoot {
                        parent_root: ProjectRootPath(format!("/x/{parent_id}")),
                        worktree_root: ProjectRootPath(format!("/x/wb/{id}")),
                    }],
                },
            },
        }
    }

    fn sorted_ids(projects: &[ProjectInfo]) -> Vec<&str> {
        projects.iter().map(|p| p.project.id.0.as_str()).collect()
    }

    /// Workbench children carry an independent per-parent sort_order
    /// sequence starting at 0, so a flat sort by raw sort_order would
    /// interleave them among top-level projects (A(0), wb(0), B(1)).
    /// The grouped sort must keep each workbench directly beneath its
    /// parent instead.
    #[test]
    fn sort_project_infos_groups_workbenches_under_parent() {
        let mut projects = vec![
            workbench_project("h-1", "wb-b", "Bench B", 0, "p-b"),
            top_level_project("h-1", "p-b", "B", 1),
            top_level_project("h-1", "p-a", "A", 0),
        ];
        sort_project_infos(&mut projects);
        assert_eq!(sorted_ids(&projects), vec!["p-a", "p-b", "wb-b"]);
    }

    /// Multiple children of one parent order by their own sort_order,
    /// and siblings of different parents never interleave.
    #[test]
    fn sort_project_infos_orders_children_per_parent() {
        let mut projects = vec![
            workbench_project("h-1", "wb-a2", "Bench A2", 1, "p-a"),
            top_level_project("h-1", "p-b", "B", 1),
            workbench_project("h-1", "wb-b1", "Bench B1", 0, "p-b"),
            workbench_project("h-1", "wb-a1", "Bench A1", 0, "p-a"),
            top_level_project("h-1", "p-a", "A", 0),
        ];
        sort_project_infos(&mut projects);
        assert_eq!(
            sorted_ids(&projects),
            vec!["p-a", "wb-a1", "wb-a2", "p-b", "wb-b1"]
        );
    }

    /// A workbench whose parent hasn't arrived yet (out-of-order
    /// upserts) sorts after all known top-level groups rather than
    /// panicking or landing somewhere arbitrary in the middle.
    #[test]
    fn sort_project_infos_pushes_orphan_workbenches_to_end() {
        let mut projects = vec![
            workbench_project("h-1", "wb-orphan", "Bench Orphan", 0, "p-missing"),
            top_level_project("h-1", "p-b", "B", 1),
            top_level_project("h-1", "p-a", "A", 0),
        ];
        sort_project_infos(&mut projects);
        assert_eq!(sorted_ids(&projects), vec!["p-a", "p-b", "wb-orphan"]);
    }

    /// Hosts stay segregated: grouping happens within a host, never
    /// across two paired hosts that reuse project ids.
    #[test]
    fn sort_project_infos_keeps_hosts_separate() {
        let mut projects = vec![
            workbench_project("h-2", "wb-2", "Bench", 0, "p-1"),
            top_level_project("h-2", "p-1", "Same Id Other Host", 0),
            top_level_project("h-1", "p-1", "First Host", 0),
        ];
        sort_project_infos(&mut projects);
        assert_eq!(projects[0].local_host_id.0, "h-1");
        assert_eq!(projects[1].local_host_id.0, "h-2");
        assert_eq!(sorted_ids(&projects), vec!["p-1", "p-1", "wb-2"]);
    }

    #[test]
    fn local_host_id_serializes_transparent() {
        let id = LocalHostId("h-1".to_owned());
        let encoded = serde_json::to_string(&id).unwrap();
        assert_eq!(encoded, "\"h-1\"");
    }

    #[test]
    fn paired_host_connection_status_maps_to_connection_status() {
        assert_eq!(
            ConnectionStatus::Connecting,
            PairedHostConnectionStatus::Connecting.into()
        );
        assert_eq!(
            ConnectionStatus::Connected,
            PairedHostConnectionStatus::Connected.into()
        );
        assert!(matches!(
            ConnectionStatus::from(PairedHostConnectionStatus::Disconnected {
                reason: "x".to_owned(),
            }),
            ConnectionStatus::Disconnected
        ));
        assert!(matches!(
            ConnectionStatus::from(PairedHostConnectionStatus::Failed {
                code: protocol::MobileAccessErrorCode::TransportFailed,
                message: "boom".to_owned(),
            }),
            ConnectionStatus::Error(_)
        ));
    }
}
